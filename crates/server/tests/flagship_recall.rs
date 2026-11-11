//! R1 anchor-gating acceptance gates on the flagship radar corpus
//! (recall-quality roadmap R1, research/2026-11-recall-quality-optimization.md
//! §3 — the origin query 「ThoughtWorks 技术雷达中 Vibe Coding 技术有哪些变化」).
//!
//! Env-gated like `flagship_corpus.rs`/`s3_conformance.rs`: these tests need
//! material that lives outside the tree and a real model download, so they
//! refuse to run unless the env names it (run with `-- --ignored`).
//!
//! ## 1. `r1_probe_calibration` — the df ceiling's calibration oracle
//!
//! Set `VF_R1_CORPUS_DIR` to a vectorfs data dir whose `docs` bucket (or
//! `VF_R1_CORPUS_BUCKET`) already holds the radar corpus + textbook with the
//! FTS index built (`vectorfs optimize`), e.g. the acceptance dir:
//!
//! ```sh
//! VF_R1_CORPUS_DIR=/tmp/r1acc \
//!   cargo test -p vectorfs-server --test flagship_recall -- --ignored --nocapture
//! ```
//!
//! It drives the EXACT production probe face (`lexical::kernel().probe_df`
//! — the postings count of the bare term, capped at `PROBE_DF_LIMIT`)
//! against the live derived table and pins the MEASURED calibration
//! `ANCHOR_DF_CEILING`'s doc comment records: the df order
//! (vibe 1 < coding 35 ≤ ThoughtWorks 36 < 雷达 109 < 技术 336), the
//! ceiling's rare/structure split, the probe cap's saturation, and the
//! v2 head-gate counterfactuals (must=[vibe] ⇒ exactly the
//! correct-answer chunk; an AND-of-anchors verdict including ThoughtWorks
//! ⇒ the needle evicted — the measured basis of the single-rarest-anchor
//! MUST). History: the v1 probe (top-1 BM25 score) was measured to
//! INVERT this ordering (tf/length effects), which retired it.
//! Pure-CJK terms are outside the v1 analyzer's candidate set (the
//! analyzer never probes them; the raw count is printed for the record
//! only). NOTE: this reads the table through LanceDB while a daemon MAY
//! hold the app-level flock — readers are safe (the flock guards
//! serve/mcp/optimize writers, and Lance reads don't lock); make sure no
//! ingest is in flight.
//!
//! ## 2. `flagship_origin_query_anchor_gated_top5` — the therapy proof
//!
//! Set `VF_FLAGSHIP_PDF_DIR` to the radar corpus directory (vol28..32.pdf,
//! the `flagship_corpus.rs` convention; any other `*.pdf` present joins the
//! corpus under its own name — e.g. book.pdf, the textbook flooding source
//! of the nine-experiment corpus):
//!
//! ```sh
//! VF_FLAGSHIP_PDF_DIR=/tmp/flagship \
//!   [VF_FLAGSHIP_MODELS_DIR=/root/p1fix/models] \
//!   cargo test -p vectorfs-server --test flagship_recall -- --ignored --nocapture
//! ```
//!
//! The test stands a REAL `vectorfs serve` daemon (fresh temp data dir,
//! gte text embedder, fts bucket, anchor-gating at its default ON), ingests
//! the corpus under the acceptance-session key form
//! `radar/tr_technology_radar_vol_<N>_cn.pdf` (so vol32's chunk keys carry
//! the recorded `db4d1d…` hash prefix — asserted as a corpus-identity
//! guard), builds the FTS index (`vectorfs optimize`, daemon down — the
//! verb's single-writer rule), and asks the origin query in HYBRID mode.
//!
//! The acceptance bar (experiment I: the pre-R1 stack put the correct
//! chunk at hybrid rank 27): the correct-answer chunk — a vol32 chunk whose
//! page coordinate overlaps p18-19 (「自满于 AI 生成的代码」, the Vibe Coding
//! entry) — must land in the TOP 5, with the `anchors` verdict (the df
//! analysis, rarest-first; the head is the gate term) riding the response.
//! A failure here is REPORTED, never papered over: the test prints every
//! hit (rank/key/source/coord/score) and the control terms' df counts —
//! diagnose the ceiling, gate semantics or RRF fusion order from that, do
//! not loosen the bar.

use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use vectorfs_server::store::Storage;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/server/../../ is the repo root")
        .to_path_buf()
}

fn ort_lib() -> PathBuf {
    let lib = if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    };
    let p = repo_root().join("runtime-libs").join(lib);
    assert!(
        p.is_file(),
        "R1 acceptance needs the real ONNX Runtime at {} — run \
         scripts/fetch_runtime_libs.sh onnxruntime; tests refuse to skip",
        p.display()
    );
    p
}

/// The client binary sits next to the server binary in the cargo bin dir
/// (the cli_smoke convention): build the workspace before running.
fn vf_bin() -> PathBuf {
    let vf = PathBuf::from(env!("CARGO_BIN_EXE_vectorfs"))
        .parent()
        .expect("bin dir")
        .join("vf");
    assert!(
        vf.is_file(),
        "vf client binary missing at {} — build the workspace: cargo test --workspace",
        vf.display()
    );
    vf
}

const AK: &str = "VFR1ACCEPTKEY0001";
const SK: &str = "vf-r1-acceptance-secret";

// ---------------------------------------------------------------------------
// gate 1: the calibration oracle (raw probe scores on the live corpus)
// ---------------------------------------------------------------------------

/// The control terms of the R1 calibration (see `ANCHOR_PROBE_FLOOR`'s doc
/// comment in lexical.rs — these tests are its measurement record).
#[test]
#[ignore = "needs an FTS-indexed radar corpus on disk; set VF_R1_CORPUS_DIR (see module doc)"]
fn r1_probe_calibration() {
    let dir = std::env::var("VF_R1_CORPUS_DIR").expect(
        "VF_R1_CORPUS_DIR must point at a vectorfs data dir holding the corpus (see module doc)",
    );
    let bucket = std::env::var("VF_R1_CORPUS_BUCKET").unwrap_or_else(|_| "docs".to_string());
    let storage = Storage::file(&dir);

    // the bucket's first derived index (one embedder = one; multi-embedder
    // corpora: every derived table carries the same chunk texts — the probe
    // is route-independent, take the first)
    let indexes_dir = Path::new(&dir).join(format!("{bucket}-vf")).join("indexes");
    let mut index: Option<String> = None;
    for entry in std::fs::read_dir(&indexes_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", indexes_dir.display()))
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().join("index.json").is_file() {
            index = Some(name);
            break;
        }
    }
    let index = index.unwrap_or_else(|| {
        panic!(
            "no derived index under {} — ingest + `vectorfs optimize` first",
            indexes_dir.display()
        )
    });
    eprintln!("corpus: {}/{} (index {index})", dir, bucket);

    let kernel = vectorfs_server::lexical::kernel();
    // ONE runtime for every probe: lance's global compute pool outlives a
    // dropped runtime, and a SECOND runtime's queries hang on it — probe
    // through the same rt for all terms (block_on is repeat-callable)
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio rt");
    // the v2 probe IS the df count (capped at PROBE_DF_LIMIT); an
    // independent deep count cross-checks the cap
    let probe = |term: &str| {
        rt.block_on(kernel.probe_df(&storage, &bucket, &index, term))
            .unwrap_or_else(|e| panic!("probe {term:?}: {e}"))
    };
    let df_deep = |term: &str| {
        rt.block_on(storage.keyword_search(&bucket, &index, term, 2000, None))
            .unwrap_or_else(|e| panic!("df {term:?}: {e}"))
            .len() as u64
    };
    // probe latency (the R1 latency budget's measured basis, ~MAX_PROBES
    // small single-term FTS queries per analyzed query)
    let probe_ms = |term: &str| {
        let t0 = std::time::Instant::now();
        let _ = probe(term);
        t0.elapsed().as_secs_f64() * 1000.0
    };

    // The v2 calibration table on this corpus (see lexical::
    // ANCHOR_DF_CEILING's doc): the count probe reads the df order
    // directly — vibe 1 < coding 35 ≤ ThoughtWorks 36 < 雷达 109 <
    // 技术 336 — where the v1 top-1-BM25 probe measured the ORDER
    // INVERTED (ThoughtWorks above vibe: tf/length effects), the
    // finding that retired the score probe.
    let ceiling = vectorfs_server::lexical::ANCHOR_DF_CEILING;
    let cap = vectorfs_server::lexical::PROBE_DF_LIMIT as u64;
    eprintln!("ANCHOR_DF_CEILING = {ceiling}  PROBE_DF_LIMIT = {cap}");
    let _ = probe_ms("vibe"); // warm the index readers first
    let mut counts = std::collections::BTreeMap::new();
    let mut probe_total_ms = 0.0f64;
    for t in ["vibe", "coding", "ThoughtWorks", "技术", "雷达", "自满"] {
        let ms = probe_ms(t);
        probe_total_ms += ms;
        let c = probe(t);
        let deep = df_deep(t);
        eprintln!("probe[{t}] = min(df, cap) = {c}  df = {deep}  {ms:.1} ms");
        counts.insert(t.to_string(), (c, deep));
    }
    eprintln!(
        "probe latency: {probe_total_ms:.0} ms for 6 terms (indexed, warmed — the R1 \
         analysis budget is ≤ lexical::MAX_PROBES such probes per query)"
    );
    // (1) the df ORDER the v2 judgment rides (compared on the DEEP counts:
    // the probe's cap saturates high-df terms to the same value): rarest
    // anchor first, the structure words far above it
    let (vibe, vibe_df) = counts["vibe"];
    let (coding, coding_df) = counts["coding"];
    let (tw, tw_df) = counts["ThoughtWorks"];
    let (radar, radar_df) = counts["雷达"];
    let (tech, tech_df) = counts["技术"];
    assert_eq!(vibe, vibe_df);
    assert_eq!(coding, coding_df);
    assert_eq!(tw, tw_df);
    assert!(
        vibe_df < coding_df && coding_df <= tw_df && tw_df < radar_df && radar_df < tech_df,
        "the measured df order (vibe {vibe_df} < coding {coding_df} ≤ ThoughtWorks \
         {tw_df} < 雷达 {radar_df} < 技术 {tech_df}) changed — re-record \
         lexical::ANCHOR_DF_CEILING's table"
    );
    // (2) the ceiling's split on this corpus: the rare band under it, the
    // structure words over it (coding at 35 sits OVER the ceiling — the
    // origin-query verdict is [vibe] alone, which the counterfactual
    // below proves is exactly the right gate)
    assert!(
        vibe <= ceiling,
        "the rarest term must anchor: {vibe} > {ceiling}"
    );
    for (t, c) in [
        ("coding", coding),
        ("ThoughtWorks", tw),
        ("雷达", radar),
        ("技术", tech),
    ] {
        assert!(
            c > ceiling,
            "{t} counts {c} ≤ ceiling {ceiling}: a structure word would enter the \
             verdict — re-calibrate lexical::ANCHOR_DF_CEILING against this table"
        );
    }
    // (3) the cap saturates exactly where it may: the ubiquitous terms'
    // probes read as the cap (the ceiling decision needs no more)
    assert_eq!(
        counts["技术"].0, cap,
        "df 336 saturates the probe cap: min(df, cap) = cap"
    );
    // CJK terms are outside the v1 analyzer's candidate set: the analyzer
    // emits zero candidates for them, so the gate can never see them —
    // the raw count is recorded for the ceiling's margin discussion only.
    // (The sample's only Latin run, "AI", is its sole candidate.)
    assert_eq!(
        vectorfs_server::lexical::anchor_candidates("自满于 AI 生成的代码"),
        vec!["AI".to_string()],
        "the analyzer's candidate set is Latin/quoted-only by construction"
    );
    assert!(
        vectorfs_server::lexical::anchor_candidates("自满于生成的代码").is_empty(),
        "pure CJK analyzes to zero candidates — graceful degradation"
    );
    // THE PRODUCTION GATE SHAPE, verified on the live corpus (this was the
    // v1 counterfactual; under v2 it is what the query face actually runs):
    // MUST = the verdict head = the single rarest anchor {vibe} — and that
    // gate returns EXACTLY the correct-answer chunk as the lexical arm's
    // sole hit. The gated-with-all-three query is shown beside it as the
    // measured reason the head-gate ruling exists: the needle carries
    // vibe+coding but NOT ThoughtWorks, so an AND-of-anchors verdict that
    // includes ThoughtWorks evicts it (zero hits on this corpus — the same
    // straddle shape the f1 fixture measures at df=1).
    let gate_vibe = rt
        .block_on(storage.keyword_search_gated(
            &bucket,
            &index,
            &["vibe".to_string()],
            "ThoughtWorks 技术雷达中 Vibe Coding 技术有哪些变化",
            10,
            None,
        ))
        .expect("gated must=[vibe]");
    eprintln!(
        "counterfactual must=[vibe]: {:?}",
        gate_vibe.iter().map(|h| h.key.as_str()).collect::<Vec<_>>()
    );
    assert_eq!(
        gate_vibe.len(),
        1,
        "df=1: the needle is the only vibe chunk"
    );
    assert!(
        gate_vibe[0].key.ends_with("-text-47"),
        "the sole vibe chunk is the correct-answer chunk: {:?}",
        gate_vibe[0].key
    );
    let gate_all = rt
        .block_on(storage.keyword_search_gated(
            &bucket,
            &index,
            &[
                "ThoughtWorks".to_string(),
                "coding".to_string(),
                "vibe".to_string(),
            ],
            "ThoughtWorks 技术雷达中 Vibe Coding 技术有哪些变化",
            10,
            None,
        ))
        .expect("gated must=[all]");
    eprintln!(
        "counterfactual must=[ThoughtWorks,coding,vibe]: {} hits (the needle lacks \
         ThoughtWorks — an AND-of-anchors verdict evicts it; the v2 gate takes \
         the head for exactly this reason)",
        gate_all.len()
    );
    assert!(
        !gate_all.iter().any(|h| h.key.ends_with("-text-47")),
        "an AND-of-anchors verdict including ThoughtWorks evicts the needle — \
         the measured basis of the v2 head-gate ruling"
    );
}

// ---------------------------------------------------------------------------
// gate 2: the origin query through a real daemon (the therapy proof)
// ---------------------------------------------------------------------------

struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Daemon {
    fn spawn(data_dir: &Path, port: u16, log_name: &str) -> Daemon {
        let log = data_dir.parent().unwrap().join(log_name);
        let stderr = std::fs::File::create(&log).expect("create log file");
        let child = Command::new(env!("CARGO_BIN_EXE_vectorfs"))
            .args([
                "serve",
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--port",
                &port.to_string(),
            ])
            .env("VF_ORT_PATH", ort_lib())
            .stdout(Stdio::from(stderr.try_clone().expect("clone log")))
            .stderr(stderr)
            .spawn()
            .expect("spawn vectorfs serve");
        Daemon { child, log }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn dump_log_tail(&self, context: &str) {
        eprintln!("--- serve log tail ({context}, {}):", self.log.display());
        if let Ok(mut f) = std::fs::File::open(&self.log) {
            let mut buf = String::new();
            let _ = f.read_to_string(&mut buf);
            let lines: Vec<&str> = buf.lines().collect();
            for line in lines.iter().rev().take(30).rev() {
                eprintln!("    {line}");
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.kill();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_healthy(port: u16, deadline: Instant) {
    while Instant::now() < deadline {
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            let _ = s.write_all(b"GET /health HTTP/1.1\r\nhost: l\r\nconnection: close\r\n\r\n");
            let mut buf = [0u8; 512];
            if let Ok(n) = s.read(&mut buf)
                && std::str::from_utf8(&buf[..n]).is_ok_and(|r| r.contains("\"ok\":true"))
            {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "serve never became healthy on port {port} (first run downloads the gte model, ~1.2 GiB — set VF_FLAGSHIP_MODELS_DIR to a warm cache)"
    );
}

/// Isolated XDG_CONFIG_HOME so a developer's real client config cannot leak
/// credentials into the test daemon (the cli_smoke discipline).
fn empty_config_home() -> PathBuf {
    let d = std::env::temp_dir().join(format!("vf-r1-acc-no-config-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn vf(port: u16, args: &[&str]) -> Output {
    Command::new(vf_bin())
        .args(args)
        .env("VF_ENDPOINT", format!("http://127.0.0.1:{port}"))
        .env("VF_ACCESS_KEY", AK)
        .env("VF_SECRET_KEY", SK)
        .env("XDG_CONFIG_HOME", empty_config_home())
        .output()
        .expect("run vf")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn config_toml(data_dir: &Path, models_dir: &Path) -> String {
    let _ = data_dir;
    format!(
        r#"version = 1
region = "us-east-1"
account_id = "000000000000"
access_key_id = "{AK}"
secret_access_key = "{SK}"
models_dir = "{models}"

[embedder.text]
kind = "ort"
model = "gte-multilingual-base"

[extractor.pdf]
kind = "pdf-text"

[ingest.docs]
extract = {{ pdf = "pdf" }}
chunk = {{ kind = "token-window", size = 512, overlap = 64 }}
embedders = ["text"]
merge = "rrf"
fts = true
scan_interval_secs = 2
"#,
        models = models_dir.display()
    )
}

/// The acceptance-session key form: `vol32.pdf` (the flagship_corpus.rs
/// fetch shape) ingests as `radar/tr_technology_radar_vol_32_cn.pdf`, so
/// vol32's chunk keys carry the recorded `db4d1d…` object-key hash. Other
/// files keep their plain name.
fn corpus_key(file: &Path) -> String {
    let name = file.file_name().unwrap().to_string_lossy().into_owned();
    if let Some(n) = name
        .strip_prefix("vol")
        .and_then(|r| r.strip_suffix(".pdf"))
        .and_then(|n| n.parse::<u32>().ok())
    {
        return format!("radar/tr_technology_radar_vol_{n}_cn.pdf");
    }
    name
}

#[test]
#[ignore = "needs the flagship corpus on disk; set VF_FLAGSHIP_PDF_DIR (see module doc)"]
fn flagship_origin_query_anchor_gated_top5() {
    let corpus = std::env::var("VF_FLAGSHIP_PDF_DIR")
        .expect("VF_FLAGSHIP_PDF_DIR must point at the radar corpus directory (see module doc)");
    let corpus = PathBuf::from(corpus);
    let mut pdfs: Vec<PathBuf> = std::fs::read_dir(&corpus)
        .unwrap_or_else(|e| panic!("read {}: {e}", corpus.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")))
        .collect();
    pdfs.sort();
    assert!(!pdfs.is_empty(), "no *.pdf under {}", corpus.display());

    let tmp = tempfile::tempdir().expect("tmp");
    let data_dir = tmp.path().join("data");
    let models_dir = match std::env::var("VF_FLAGSHIP_MODELS_DIR") {
        Ok(m) => PathBuf::from(m),
        Err(_) => data_dir.join(".vectorfs/models"), // hermetic (downloads ~1.2 GiB)
    };
    std::fs::create_dir_all(data_dir.join(".vectorfs")).unwrap();
    std::fs::write(
        data_dir.join(".vectorfs/vectorfs.toml"),
        config_toml(&data_dir, &models_dir),
    )
    .unwrap();

    let port = free_port();
    let mut daemon = Daemon::spawn(&data_dir, port, "r1-serve1.log");
    // first run downloads the gte model into models_dir (unless warm)
    wait_healthy(port, Instant::now() + Duration::from_secs(1500));

    // ingest the corpus under the acceptance key form, foreground per file
    for pdf in &pdfs {
        let key = corpus_key(pdf);
        eprintln!("put {} as {key}", pdf.display());
        let o = vf(
            port,
            &[
                "put",
                pdf.to_str().unwrap(),
                "--bucket",
                "docs",
                "--key",
                &key,
                "--timeout",
                "1500",
            ],
        );
        assert!(
            o.status.success(),
            "put {key} failed\nstdout:\n{}\nstderr:\n{}",
            out(&o),
            err(&o)
        );
    }

    // corpus-identity guard: vol32's chunk keys carry the recorded hash
    let vol32 = pdfs
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n == "vol32.pdf"))
        .expect("the corpus must hold vol32.pdf (the correct-answer volume)");
    let o = vf(
        port,
        &["chunks", "--bucket", "docs", &corpus_key(vol32), "--json"],
    );
    assert!(o.status.success(), "{}", err(&o));
    let chunks: serde_json::Value = serde_json::from_str(&out(&o)).expect("chunks json");
    let hash = chunks["hash"].as_str().expect("hash member").to_string();
    assert!(
        hash.starts_with("db4d1d"),
        "vol32's object-key hash is {hash}, expected the recorded db4d1d… prefix \
         (the acceptance-session key form radar/tr_technology_radar_vol_32_cn.pdf)"
    );
    eprintln!(
        "vol32 hash {hash}: corpus identity confirmed ({} chunks)",
        chunks["count"]
    );

    // the FTS index: optimize is a single-writer verb — daemon down, run,
    // daemon back up (the operator flow)
    daemon.kill();
    let opt = Command::new(env!("CARGO_BIN_EXE_vectorfs"))
        .args(["optimize", "--data-dir", data_dir.to_str().unwrap()])
        .env("VF_ORT_PATH", ort_lib())
        .output()
        .expect("run vectorfs optimize");
    assert!(
        opt.status.success(),
        "optimize failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&opt.stdout),
        String::from_utf8_lossy(&opt.stderr)
    );
    eprintln!(
        "optimize:\n{}",
        String::from_utf8_lossy(&opt.stdout).trim_end()
    );
    let daemon = Daemon::spawn(&data_dir, port, "r1-serve2.log");
    wait_healthy(port, Instant::now() + Duration::from_secs(120));

    // diagnostics first: the control terms' measured df split on THIS
    // corpus (keyword top-64 hit count = the lexical::probe_df semantics;
    // gating does not change a COUNT — a gated single-term query returns
    // exactly that term's postings)
    for t in ["vibe", "coding", "ThoughtWorks", "技术", "雷达", "自满"] {
        let o = vf(
            port,
            &[
                "search", t, "docs", "--mode", "keyword", "--top-k", "64", "--json",
            ],
        );
        let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap_or_else(|e| {
            panic!(
                "probe {t} json: {e}\nstdout:\n{}\nstderr:\n{}",
                out(&o),
                err(&o)
            )
        });
        let count = v["hits"].as_array().map(Vec::len).unwrap_or(0);
        eprintln!("probe[{t}] df = {count}");
    }

    // THE acceptance query: hybrid mode, default anchor gating. Diagnostics
    // print FIRST (the honest report needs every number even when the bar
    // is missed); the asserts below are the BAR and stay strict — a
    // failure here is reported with the printed table, never papered over.
    let o = vf(
        port,
        &[
            "search",
            "ThoughtWorks 技术雷达中 Vibe Coding 技术有哪些变化",
            "docs",
            "--mode",
            "hybrid",
            "--top-k",
            "10",
            "--json",
        ],
    );
    assert!(o.status.success(), "{}", err(&o));
    let v: serde_json::Value =
        serde_json::from_str(&out(&o)).unwrap_or_else(|e| panic!("search json: {e}\n{}", out(&o)));
    eprintln!("anchors verdict: {:?}", v["anchors"]);
    let hits = v["hits"].as_array().expect("hits");
    assert!(!hits.is_empty(), "the hybrid query must answer");

    // find the correct-answer chunk: a vol32 chunk addressing p18-19 (the
    // Vibe Coding entry「自满于 AI 生成的代码」)
    let mut rank_of_correct: Option<usize> = None;
    for (i, h) in hits.iter().enumerate() {
        let key = h["key"].as_str().unwrap_or("?");
        let src = h["location"]["key"].as_str().unwrap_or("?");
        let coord = &h["location"]["coord"];
        let page = coord["page"].as_object();
        let (ps, pe) = page
            .map(|p| {
                (
                    p["start"].as_u64().unwrap_or(0),
                    p["end"].as_u64().unwrap_or(0),
                )
            })
            .unwrap_or((0, 0));
        eprintln!(
            "hit[{i}] {key} {src} p{ps}-{pe} score {:?}",
            h["score"].as_f64()
        );
        if key.starts_with(&hash) && ps <= 19 && pe >= 18 {
            rank_of_correct = Some(i);
        }
    }
    let rank = rank_of_correct.unwrap_or_else(|| {
        daemon.dump_log_tail("correct chunk absent from top-10");
        panic!(
            "the correct-answer chunk (vol32 {hash}…, p18-19) is not in the top 10 — \
             pre-R1 it ranked 27; see probe diagnostics above and the hit list"
        )
    });
    // THE BAR (both strict, no silent loosening):
    // 1. the anchor verdict — under the v2 count probe the origin query's
    //    rarest term Vibe (df 1, the query's own spelling — the index
    //    tokenizer folds case at MATCH time, the verdict keeps the query's)
    //    is the verdict head and its ONLY sub-ceiling candidate on this
    //    corpus shape (coding/ThoughtWorks df in the dozens sit over the
    //    ceiling; the exact numbers print in the probe diagnostics above —
    //    a mismatch here means the corpus's df table drifted, re-run
    //    r1_probe_calibration and re-calibrate lexical::ANCHOR_DF_CEILING
    //    before touching anything else).
    assert_eq!(
        v["anchors"],
        serde_json::json!(["Vibe"]),
        "the origin query's anchor verdict — rarest-first, head = the gate (see \
         the probe diagnostics above and lexical::ANCHOR_DF_CEILING's table)"
    );
    // 2. the rank bar — top-5 (experiment I's pre-R1 baseline: rank 27)
    assert!(
        rank < 5,
        "the correct-answer chunk sits at hybrid rank {} ({}-indexed) — the acceptance \
         bar is top-5 (experiment I baseline: rank 27); diagnose probes/fusion from the \
         printed diagnostics before touching anything",
        rank + 1,
        rank
    );
    eprintln!(
        "ACCEPTED: correct chunk at hybrid rank {} (top-5)",
        rank + 1
    );
}
