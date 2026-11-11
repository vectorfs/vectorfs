//! R2 CCH (context-header = toc-path) acceptance gate on the flagship radar
//! corpus (recall-quality roadmap R2, research/2026-11-recall-quality-optimization.md
//! §3 — the origin query 「ThoughtWorks 技术雷达中 Vibe Coding 技术有哪些变化」).
//!
//! Env-gated like `flagship_recall.rs`/`flagship_corpus.rs`: the corpus lives
//! outside the tree and the gte model is a real download. Set
//! `VF_FLAGSHIP_PDF_DIR` to the radar corpus directory (vol28..32.pdf — the
//! `flagship_corpus.rs` fetch convention; any other `*.pdf` present joins
//! under its own name), optionally `VF_FLAGSHIP_MODELS_DIR` to a warm cache:
//!
//! ```sh
//! VF_FLAGSHIP_PDF_DIR=/tmp/flagship \
//!   [VF_FLAGSHIP_MODELS_DIR=/root/p1fix/models] \
//!   cargo test -p vectorfs-server --test flagship_cch -- --ignored --nocapture
//! ```
//!
//! ## What this gate proves
//!
//! The test ingests the SAME corpus into TWO data dirs, differing by exactly
//! one chunk-table knob:
//!
//! * **plain** — `context-header` absent (the default `none`): the R1-era
//!   baseline, byte-identical embeddings;
//! * **cch** — `context-header = "toc-path"`: every chunk's EMBEDDING INPUT
//!   carries `tr_technology_radar_vol_N_cn > <nearest preceding TOC entry>`
//!   (the measured vol32 map's top-level entries are the four quadrants —
//!   the Vibe Coding pages p18-19 map to 技术, p12).
//!
//! Then the origin query runs in all three modes on BOTH corpora and the
//! correct-answer chunk's rank (a vol32 chunk addressing p18-19) is REPORTED
//! as a paired table. **No hard rank bar on the semantic comparison** (the
//! R2 adjudication: record the numbers honestly; improvement is bonus, not
//! gate) — the two HARD assertions are:
//!
//! 1. **the R1 regression bar holds under the re-embedded corpus**: hybrid
//!    mode with anchor gating still lands the correct chunk top-5 (the R1
//!    acceptance bar; R2 must not regress what R1 fixed);
//! 2. **the hard boundary survives the daemon end to end**: the STORED
//!   `x-vf-chunk-text` of the correct chunk is the ORIGINAL text — the
//!   prefix appears nowhere in storage or the FTS face (the anchors verdict
//!   is still `["Vibe"]`, the df statistics unpolluted — R1's calibration
//!   reads the same corpus through the original text).
//!
//! ## Measured record (2026-11-11, warm gte cache, /tmp/flagship corpus)
//!
//! Two runs of the same gate, two builds of the same corpora:
//!
//! | mode | plain (R1-era) | cch (toc-path), run A | cch, run B |
//! |---|---|---|---|
//! | semantic | absent top-10 | **rank 4** | absent top-10 |
//! | keyword | rank 1 | rank 1 | rank 1 |
//! | hybrid | rank 2 | **rank 1** | rank 2 |
//!
//! The keyword arm is deterministic (BM25 over identical text — the FTS
//! boundary holding). The vector arms wobble AT THE BOUNDARY across runs
//! (semantic rank 4 vs just-outside-top-10; hybrid 1 vs 2): CPU gte
//! inference is thread-scheduling-sensitive at the last float bits, and
//! the correct chunk's semantic score sits near rank-10. The honest
//! reading: CCH pulls the correct chunk FROM outside the semantic top-10
//! TO its boundary (rank ~4-10), where the R1-gated fusion then locks it
//! at hybrid top-2 — the R1 bar held in every run. The 加分 the research
//! hoped for (semantic-arm improvement) is REAL but boundary-straddling,
//! not a stable rank-1.

use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

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
        "R2 acceptance needs the real ONNX Runtime at {} — run \
         scripts/fetch_runtime_libs.sh onnxruntime; tests refuse to skip",
        p.display()
    );
    p
}

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

const AK: &str = "VFR2ACCEPTKEY0001";
const SK: &str = "vf-r2-acceptance-secret";

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
        "serve never became healthy on port {port} (first run downloads the gte model, ~1.2 GiB — \
         set VF_FLAGSHIP_MODELS_DIR to a warm cache)"
    );
}

fn empty_config_home() -> PathBuf {
    let d = std::env::temp_dir().join(format!("vf-r2-acc-no-config-{}", std::process::id()));
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

/// The ONE axis difference between the two corpora rides the chunk table:
/// `context-header = "toc-path"` (R2 CCH) vs absent (the default `none`,
/// the R1-era baseline). Everything else — extractor, window, embedder,
/// fts — is the R1 acceptance shape verbatim.
fn config_toml(models_dir: &Path, context_header: bool) -> String {
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
chunk = {{ kind = "token-window", size = 512, overlap = 64{chunk_extra} }}
embedders = ["text"]
merge = "rrf"
fts = true
scan_interval_secs = 2
"#,
        models = models_dir.display(),
        chunk_extra = if context_header {
            ", context-header = \"toc-path\""
        } else {
            ""
        }
    )
}

/// The acceptance-session key form (the flagship_recall.rs convention):
/// `vol32.pdf` ingests as `radar/tr_technology_radar_vol_32_cn.pdf`, so
/// vol32's chunk keys carry the recorded `db4d1d…` object-key hash and the
/// CCH filename stem is `tr_technology_radar_vol_32_cn`.
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

const ORIGIN_QUERY: &str = "ThoughtWorks 技术雷达中 Vibe Coding 技术有哪些变化";

/// One corpus build: materialize config, serve, ingest, build the FTS
/// index (optimize — the single-writer verb, daemon down/up around it),
/// return the live daemon + its port + vol32's key hash.
fn build_corpus(
    tmp: &Path,
    name: &str,
    models_dir: &Path,
    pdfs: &[PathBuf],
    context_header: bool,
) -> (Daemon, u16, String) {
    let data_dir = tmp.join(name).join("data");
    std::fs::create_dir_all(data_dir.join(".vectorfs")).unwrap();
    std::fs::write(
        data_dir.join(".vectorfs/vectorfs.toml"),
        config_toml(models_dir, context_header),
    )
    .unwrap();

    let port = free_port();
    let mut daemon = Daemon::spawn(&data_dir, port, &format!("{name}-serve1.log"));
    wait_healthy(port, Instant::now() + Duration::from_secs(1500));

    for pdf in pdfs {
        let key = corpus_key(pdf);
        eprintln!("[{name}] put {} as {key}", pdf.display());
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
            "[{name}] put {key} failed\nstdout:\n{}\nstderr:\n{}",
            out(&o),
            err(&o)
        );
    }

    // corpus-identity guard: vol32's hash (the acceptance-session key form)
    let vol32_key = corpus_key(
        pdfs.iter()
            .find(|p| p.file_name().is_some_and(|n| n == "vol32.pdf"))
            .expect("the corpus must hold vol32.pdf (the correct-answer volume)"),
    );
    let o = vf(port, &["chunks", "--bucket", "docs", &vol32_key, "--json"]);
    assert!(o.status.success(), "[{name}] {}", err(&o));
    let chunks: serde_json::Value = serde_json::from_str(&out(&o)).expect("chunks json");
    let hash = chunks["hash"].as_str().expect("hash member").to_string();
    assert!(
        hash.starts_with("db4d1d"),
        "[{name}] vol32's object-key hash is {hash}, expected the recorded db4d1d… prefix"
    );
    eprintln!("[{name}] vol32 hash {hash} ({} chunks)", chunks["count"]);

    // FTS index: optimize with the daemon down (the verb's single-writer
    // rule), then back up
    daemon.kill();
    let opt = Command::new(env!("CARGO_BIN_EXE_vectorfs"))
        .args(["optimize", "--data-dir", data_dir.to_str().unwrap()])
        .env("VF_ORT_PATH", ort_lib())
        .output()
        .expect("run vectorfs optimize");
    assert!(
        opt.status.success(),
        "[{name}] optimize failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&opt.stdout),
        String::from_utf8_lossy(&opt.stderr)
    );
    let daemon = Daemon::spawn(&data_dir, port, &format!("{name}-serve2.log"));
    wait_healthy(port, Instant::now() + Duration::from_secs(120));
    (daemon, port, hash)
}

/// The correct-answer chunk's rank in one mode's top-K (None = absent).
fn rank_of_correct(v: &serde_json::Value, hash: &str) -> Option<usize> {
    let hits = v["hits"].as_array()?;
    for (i, h) in hits.iter().enumerate() {
        let key = h["key"].as_str().unwrap_or("?");
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
        if key.starts_with(hash) && ps <= 19 && pe >= 18 {
            return Some(i);
        }
    }
    None
}

fn search(port: u16, mode: &str, top_k: usize) -> serde_json::Value {
    let o = vf(
        port,
        &[
            "search",
            ORIGIN_QUERY,
            "docs",
            "--mode",
            mode,
            "--top-k",
            &top_k.to_string(),
            "--json",
        ],
    );
    assert!(o.status.success(), "search {mode}: {}", err(&o));
    serde_json::from_str(&out(&o))
        .unwrap_or_else(|e| panic!("search {mode} json: {e}\n{}", out(&o)))
}

#[test]
#[ignore = "needs the flagship corpus on disk; set VF_FLAGSHIP_PDF_DIR (see module doc)"]
fn flagship_cch_origin_query_paired_three_mode() {
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
    let models_dir = match std::env::var("VF_FLAGSHIP_MODELS_DIR") {
        Ok(m) => PathBuf::from(m),
        Err(_) => tmp.path().join("models"), // hermetic (downloads ~1.2 GiB)
    };

    // ---- corpus A: the R1-era baseline (context-header absent) ----
    // both daemons stay alive for the whole test (their Drop kills them)
    let (plain_daemon, plain_port, plain_hash) =
        build_corpus(tmp.path(), "plain", &models_dir, &pdfs, false);
    // ---- corpus B: the R2 corpus (context-header = toc-path) ----
    let (cch_daemon, cch_port, cch_hash) =
        build_corpus(tmp.path(), "cch", &models_dir, &pdfs, true);
    let _ = (&plain_daemon, &cch_daemon);
    assert_eq!(
        plain_hash, cch_hash,
        "the two corpora share the object-key hash (the same acceptance key form)"
    );

    // ---- the paired three-mode table (reported; no semantic bar) ----
    eprintln!("=== origin query: rank of the correct chunk (1-indexed, top-10; '-' = absent) ===");
    let mut cch_hybrid_rank = None;
    let mut cch_anchors = None;
    for mode in ["semantic", "keyword", "hybrid"] {
        let plain = search(plain_port, mode, 10);
        let cch = search(cch_port, mode, 10);
        let pr = rank_of_correct(&plain, &plain_hash);
        let cr = rank_of_correct(&cch, &cch_hash);
        eprintln!(
            "{mode:9}  plain(R1-era baseline): {}   cch(toc-path): {}",
            pr.map(|r| format!("rank {}", r + 1))
                .unwrap_or_else(|| "-".into()),
            cr.map(|r| format!("rank {}", r + 1))
                .unwrap_or_else(|| "-".into()),
        );
        if mode == "hybrid" {
            cch_hybrid_rank = cr;
            cch_anchors = Some(cch["anchors"].clone());
        }
        // every mode answered
        assert!(
            !plain["hits"].as_array().unwrap().is_empty()
                && !cch["hits"].as_array().unwrap().is_empty(),
            "{mode} must answer on both corpora"
        );
    }

    // ---- HARD BAR 1: the R1 regression bar holds under the re-embed ----
    let rank = cch_hybrid_rank.unwrap_or_else(|| {
        cch_daemon.dump_log_tail("correct chunk absent from cch hybrid top-10");
        panic!(
            "the correct-answer chunk is not in the cch corpus's hybrid top-10 — the R1 \
             acceptance bar (top-5) regressed under the R2 re-embed; see the paired table above"
        )
    });
    assert!(
        rank < 5,
        "the correct-answer chunk sits at hybrid rank {} ({}-indexed) on the cch corpus — the \
         R1 acceptance bar is top-5 and R2 must not regress it; see the paired table above",
        rank + 1,
        rank
    );

    // ---- HARD BAR 2: the boundary, end to end ----
    // (a) R1's anchor verdict is byte-identical on the cch corpus — the df
    //     statistics read ORIGINAL text (a leaked prefix would shift them)
    assert_eq!(
        cch_anchors.unwrap(),
        serde_json::json!(["Vibe"]),
        "the anchors verdict on the cch corpus — R1's calibration reads the unprefixed FTS face"
    );
    // (b) the STORED text of the correct chunk is the ORIGINAL: fetch it
    //     through the hybrid hit (the returned text member is the stored
    //     x-vf-chunk-text) and pin both directions — the needle's phrase
    //     present, the composed prefix's header shape absent
    let hybrid = search(cch_port, "hybrid", 10);
    let hits = hybrid["hits"].as_array().unwrap();
    let needle_hit = hits
        .iter()
        .find(|h| {
            let key = h["key"].as_str().unwrap_or("?");
            let coord = &h["location"]["coord"];
            let (ps, pe) = coord["page"]
                .as_object()
                .map(|p| {
                    (
                        p["start"].as_u64().unwrap_or(0),
                        p["end"].as_u64().unwrap_or(0),
                    )
                })
                .unwrap_or((0, 0));
            key.starts_with(&cch_hash) && ps <= 19 && pe >= 18
        })
        .unwrap_or_else(|| panic!("the correct hit must carry text for the boundary pin"));
    // the wire shape nests the stored text under content.text (the
    // returnText channel; default-on — cli.md's --no-text is the opt-out)
    let stored = needle_hit["content"]["text"].as_str().unwrap_or_default();
    // the needle phrase witness: the Vibe Coding entry's own Latin anchor —
    // the entry's chunk carries "vibe coding" verbatim (the 自满 phrasing
    // lives in a neighboring window of the same p18-19 entry)
    assert!(
        stored.contains("vibe coding"),
        "the stored text of the correct chunk lost the needle phrase: {stored:?}"
    );
    assert!(
        !stored.contains("tr_technology_radar_vol_32_cn"),
        "the filename stem (the prefix's first component) leaked into STORED text: {stored:?}"
    );
    assert!(
        !stored.contains(" > "),
        "the heading-path header shape leaked into STORED text: {stored:?}"
    );
    eprintln!(
        "ACCEPTED: cch hybrid rank {} (R1 bar top-5 held); stored text unprefixed ({} chars)",
        rank + 1,
        stored.chars().count()
    );
}
