//! T16 — the synthetic narrative golden corpus (R9,
//! docs/design/2026-11-narrative-recall-mechanisms.md §1 D-11, §2 T16).
//!
//! The corpus the narrative-recall mechanism tasks measure themselves
//! against: a fully SYNTHETIC Chinese novel (zero copyright, D-4/F-12 —
//! nothing real is quoted, every sentence below is authored for this
//! fixture), generated IN-TEST from one fixed explicit seed — nothing big
//! is committed (the `golden_corpus.rs` huge-class discipline).
//!
//! ## Shape
//!
//! * ONE .epub (built in-test with the same zip discipline as the epub
//!   kind's own unit builder): 3 卷 × 60 章 = 180 spine chapters, natural
//!   order == spine order, so **section = the global chapter number**
//!   (F-6: section is the container-walk child ordinal, 1-based, one per
//!   spine chapter that yields parts — every chapter does).
//! * Entity table: 主角 every chapter; 配角/地名/组织 mid-band by an exact
//!   period/phase policy (exactly one mention in each due chapter, zero
//!   elsewhere); 30 one-shot entities at exactly one chapter each.
//! * **10 planted events, the density-difference model (FM2's faithful
//!   structure)**: each event has ONE occurrence chapter where the query
//!   keyword appears 1–2 times (lexically findable), and 3–8 recap
//!   chapters — all with section numbers STRICTLY GREATER than the
//!   occurrence chapter — where the same keyword appears 3–6 times
//!   (higher tf: the scene can be HIT but its ranking is FLOODED by
//!   recaps). One event is a Latin identifier (`MeridianVow`) — the F1
//!   regression guard: v1's only candidate shape.
//! * Special queries: a structure-word flood query (T17's 恒门 pin — v2
//!   retired the "all-structure ⇒ zero anchors" escape), a
//!   all-saturated-tie query (three quoted coined place names at 72/84/96
//!   chapters each — every probe reads the cap 64, so the head anchor is
//!   only decidable by T17's stage-two exact counts), and a long query
//!   (>8 candidates before the rare one-shot name — the MAX_PROBES
//!   truncation semantics pin).
//! * A cross-source decoy document (a structure-word-dense "reader notes"
//!   .txt ingested into the SAME bucket): the FM4 observation material.
//!
//! ## Two hygienes the whole fixture rides (and the self-checks enforce)
//!
//! The lance FTS face probes a plain string as a `MatchQuery` with
//! `Operator::Or` (verified against lance-index 14.0.2: `Operator::default()
//! = Or`) — a multi-token probe counts the UNION of its token postings. So
//! for a quoted CJK keyword like "潮汐灯" to probe as its own event's chunk
//! count, no keyword CHARACTER may appear anywhere else in the corpus:
//!
//! 1. **keyword-char quarantine** — every character of every event keyword
//!    occurs only inside that event's own chapters (occurrence + recaps;
//!    self-checked — there, partial words like 琴声 in a recap are harmless,
//!    those chunks already carry the keyword). Tie terms are stricter:
//!    their characters occur ONLY inside the term's instances anywhere
//!    (their df must equal the residue-rule coverage exactly).
//! 2. **query zero-words** — every non-keyword content word of a fixture
//!    query has ZERO occurrences in the NOVEL and in the DECOY
//!    (self-checked both — the decoy half was added after the first T17
//!    env run caught it leaking a zero-word), so under T17's argmin-df
//!    head anchor the QUOTED keyword is trivially the rarest candidate
//!    and the head.
//!
//! ## Test gates
//!
//! Immediately green (no model, no daemon): determinism (same seed ⇒
//! byte-identical epub/decoy), corpus structure self-checks (entity bands,
//! event density model, quarantine/zero-word hygienes), the REAL-extractor
//! round-trip (`EpubExtractor::unpack` → `HtmlExtractor::extract` keeps
//! titles, refrains and exact keyword counts), and the T17 candidate
//! shape (`lexical::anchor_candidates` on the fixture queries — quoted
//! keyword first, CJK non-empty, the long-query truncation).
//!
//! Env-gated, flagship-suite discipline (`flagship_recall.rs`): the
//! full-chain INDEXED gate and the T17/T18 behavior pins all need a
//! REAL embedder model. They are `#[ignore]`d for `cargo test` and REFUSE
//! to run silently: without `VF_NARRATIVE_MODELS_DIR` set they panic with
//! the fix (never a quiet skip). Run with:
//!
//! ```sh
//! VF_NARRATIVE_MODELS_DIR=/path/to/models \
//!   cargo test -p vectorfs-server --test narrative_corpus -- --ignored --nocapture
//! ```
//!
//! * `VF_NARRATIVE_MODELS_DIR` — a models dir holding
//!   `gte-multilingual-base` (the flagship acceptance cache works).
//! * `VF_NARRATIVE_KEEP_DIR` (optional) — build the data dir AT this path
//!   instead of a tempdir, persisting the indexed corpus for reuse (T17's
//!   `r1_probe_calibration` v2 extension can point `VF_R1_CORPUS_DIR` at
//!   it; the generated epub/decoy are copied in beside it).

use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use vectorfs_server::pipeline::chunk::token_window::TokenWindowChunker;
use vectorfs_server::pipeline::extract::epub::EpubExtractor;
use vectorfs_server::pipeline::extract::html::HtmlExtractor;
use vectorfs_server::pipeline::{Chunker, Extractor};
use vectorfs_server::store::Storage;

// ---------------------------------------------------------------------------
// deterministic RNG — splitmix64, written test-side (F-12: std's
// DefaultHasher is NOT stable across releases and must not be used; the
// generator's randomness is this little state machine and nothing else)
// ---------------------------------------------------------------------------

/// The one seed of record for the T16 corpus (T16 → …0016).
const SEED: u64 = 0x5EED_0000_0000_0016;

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
    /// Pick a &str table entry (the template pools are all &str tables).
    fn pick_str<'a>(&mut self, xs: &'a [&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }
}

// ---------------------------------------------------------------------------
// vocabulary tables (the corpus's cast — all coined, pairwise
// non-substring so substring counting is unambiguous per name)
// ---------------------------------------------------------------------------

const VOLUMES: usize = 3;
const CHAPTERS_PER_VOLUME: u32 = 60;
const CHAPTERS: u32 = VOLUMES as u32 * CHAPTERS_PER_VOLUME; // 180

/// 主角: every chapter (the opening sentence always names him).
const PROTAGONIST: &str = "沈观澜";

/// 配角 (name, period, phase): due iff (chapter + phase) % period == 0, and
/// then mentioned EXACTLY once (the self-check enforces the exact band).
const SUPPORTING: [(&str, u32, u32); 6] = [
    ("裴照雨", 4, 1),
    ("祝云旌", 5, 2),
    ("阿蘅", 6, 0),
    ("陆书白", 7, 3),
    ("棠九", 9, 5),
    ("贺知秋", 11, 7),
];

/// 地名, the same policy (旧城 60 chapters, 南浦 45, 津渡码头 36, 青崖寺 30,
/// 回生药房 ~23 — the mid band; 旧城/南浦/… also ride the long query).
/// 桥 is deliberately NOT in any place name (百步桥 owns it — tie-term
/// quarantine).
const PLACES: [(&str, u32, u32); 5] = [
    ("旧城", 3, 0),
    ("南浦", 4, 1),
    ("津渡码头", 5, 2),
    ("青崖寺", 6, 3),
    ("回生药房", 8, 5),
];

/// 组织, same policy (low-mid band).
const ORGS: [(&str, u32, u32); 3] = [("盐运司", 7, 2), ("巡检司", 10, 4), ("拾遗会", 13, 6)];

/// 一次性实体: entity i lives in chapter (i*6) % 180 + 5 — 30 distinct
/// chapters, one mention each. `谢闻笛` (index 0) is the long query's rare
/// name (df = one chapter by construction).
const ONESHOTS: [&str; 30] = [
    "谢闻笛",
    "温小满",
    "顾行简",
    "白露生",
    "齐无咎",
    "岑铁衣",
    "桑晚照",
    "江采薇",
    "应不眠",
    "闻人镜",
    "柳三更",
    "米斗南",
    "屠辛夷",
    "洛停云",
    "燕辞归",
    "苏砚冰",
    "明月奴",
    "邵孤鸿",
    "曲不度",
    "阮青霜",
    "童千川",
    "华不注",
    "郁寒水",
    "单于野",
    "尹见山",
    "庄枕流",
    "骆惊鸿",
    "严九霜",
    "左听雷",
    "柯半城",
];

/// The every-chapter closing refrain — the corpus's FURNITURE: its four
/// content bigrams (夜里/城里/有人/说起) ride one chunk per chapter and so
/// saturate any df probe (the structure-word band, by construction).
const REFRAIN: &str = "夜里,城里有人说起他的事。";
const FURNITURE_BIGRAMS: [&str; 4] = ["夜里", "城里", "有人", "说起"];

/// The saturation-tie terms (term, modulus, residues-below): due iff
/// chapter % modulus < residues → 乌篷巷 72 chapters, 百步桥 84, 栖霞圩 96.
/// All ≥ 64 chapters ⇒ every stage-one probe reads the cap (the
/// two-stage-tie shape), margins ≥ 12 chapters so chunk-level df keeps the
/// chapter order. Each term gets ONE sentence in each due chapter.
const TIE_TERMS: [(&str, u32, u32); 3] = [("乌篷巷", 20, 8), ("百步桥", 15, 7), ("栖霞圩", 15, 8)];

const TIE_SENTENCES: [&str; 3] = [
    "他绕进乌篷巷时,两边的铺子刚上好板子。",
    "船到百步桥,艄公却再也不肯往前。",
    "栖霞圩的集市散得早,晌午刚过就安静了。",
];

/// 60 chapter-title words (cycled by (chapter-1) % 60; the 第N章 number
/// keeps titles unique). Scrubbed of every quarantined character.
const TITLE_WORDS: [&str; 60] = [
    "起风", "夜雨", "客栈", "归途", "棋局", "雪夜", "漕运", "私盐", "界碑", "货栈", "缝补", "押运",
    "桐油", "学徒", "竹伞", "斗笠", "芦花", "井栏", "城根", "更声", "夜航", "旧约", "借据", "荒年",
    "药市", "米价", "断缆", "枯水", "暖冬", "春汛", "秋集", "冬藏", "渡口", "夜泊", "江火", "早市",
    "晚集", "修船", "补网", "晒盐", "沙洲", "苇荡", "蟹汛", "鱼市", "沽酒", "赊账", "当铺", "质库",
    "城隍", "社戏", "社火", "龙王", "河伯", "野渡", "孤舟", "断桨", "补锅", "磨刀", "收帆", "问路",
];

/// The opening sentence pool (always contains the protagonist).
const OPENINGS: [&str; 3] = [
    "{P}天不亮就动身,露水打湿了裤脚。",
    "{P}在窗前站了很久,把要做的事又想了一遍。",
    "{P}收好行囊,这一趟要走的路不近。",
];

/// Protagonist-only filler / atmosphere sentences (no entity slots).
const P_FILLERS: [&str; 3] = [
    "{P}把旧信折好收进袖袋,没有回头的意思。",
    "{P}在窗前站了许久,把要说的话在心里过了一遍。",
    "{P}低头赶路,没理会身后的动静。",
];
const ATMOSPHERE: [&str; 3] = [
    "江面上白茫茫一片,又慢慢散开。",
    "长街上传来更声,一下,又一下。",
    "檐角的风铃响了一整夜。",
];

/// Slot fillers: {P} protagonist, {S} supporting, {L} place, {O} org,
/// {T} one-shot. Used ONLY while the matching due-queue is non-empty (an
/// entity name may never appear outside its policy chapters).
const S_FILLERS: [&str; 3] = [
    "{S}从上游带回消息,说盐价又涨了。",
    "{S}劝{P}先歇脚,剩下的事明天再谈。",
    "{S}倚着柱子看雨,看了很久。",
];
const L_FILLERS: [&str; 3] = [
    "{P}沿着{L}的长街走了半日,风里带着咸腥气。",
    "{L}的老槐树下,总有人讲些不知真假的事。",
    "{P}在{L}的茶棚里坐了半个时辰,没有等到要等的人。",
];
const O_FILLERS: [&str; 2] = [
    "{O}阶前的新榜文贴出来,围看的人把路都堵住了。",
    "{O}的院子冷清,老仆靠着廊柱打盹。",
];
const T_FILLERS: [&str; 1] = ["{T}来到镇上,只在客栈里住了一夜,天不亮又走了。"];

// ---------------------------------------------------------------------------
// the ten planted events (the density-difference model)
// ---------------------------------------------------------------------------

/// One planted event. `recaps` entries are (section, keyword_uses) with
/// uses in 3..=6; all recap sections are > `occurrence`. The `query` is the
/// natural-Chinese fixture (CJK keywords quoted — the D-8.3 quoted-span
/// candidate, probed whole); `zero_words` are the query's non-keyword
/// content words, each of which must have ZERO corpus occurrences (the
/// argmin hygiene: under T17's head anchor the quoted keyword is then
/// trivially the rarest candidate).
struct EventSpec {
    label: &'static str,
    keyword: &'static str,
    query: &'static str,
    zero_words: &'static [&'static str],
    occurrence: u32,
    /// 1 or 2 scene sentences used (each carries the keyword once).
    occurrence_uses: u32,
    scene: &'static [&'static str],
    recap_pool: &'static [&'static str],
    recaps: &'static [(u32, u32)],
}

const EVENTS: [EventSpec; 10] = [
    // E1 — the FM2 showcase event (also the A-R8 position-half subject)
    EventSpec {
        label: "潮汐灯",
        keyword: "潮汐灯",
        query: "沈观澜最早点亮\"潮汐灯\"是在哪一章",
        zero_words: &["最早", "点亮", "是在", "哪一章"],
        occurrence: 7,
        occurrence_uses: 1,
        scene: &["入夜之后,他终于燃起了潮汐灯,幽蓝的光沿着堤岸一寸一寸漫了上来。"],
        recap_pool: &[
            "酒肆里又有人说起潮汐灯,都说那晚的光像漫过堤岸的月色。",
            "老船工提起潮汐灯,总要先朝堤岸的方向望上一眼。",
            "孩子们在长阶上追跑,学着大人的口气讲潮汐灯的事。",
            "自潮汐灯那夜之后,进港的船都改走了南水道。",
        ],
        recaps: &[(19, 4), (45, 3), (88, 5), (120, 6), (166, 3)],
    },
    // E2 — the Latin identifier event (F1 regression guard: v1's candidate
    // shape works on exactly this one)
    EventSpec {
        label: "MeridianVow",
        keyword: "MeridianVow",
        query: "带 MeridianVow 标记的信最早送到哪一章",
        zero_words: &["标记", "最早", "送到", "哪一章"],
        occurrence: 12,
        occurrence_uses: 1,
        scene: &["信封的火漆上压着一枚陌生的印记,拼得出 MeridianVow 的字样。"],
        recap_pool: &[
            "后来又送来两封信,火漆上同样印着 MeridianVow,收信人却都已不在。",
            "有人描下那枚 MeridianVow 印记,四处打听出处,一无所获。",
            "坊间传言,MeridianVow 是某个远方商行的暗记。",
            "他一直留着那封 MeridianVow 的信,没有拆第二封。",
        ],
        recaps: &[(34, 4), (58, 3), (96, 5), (141, 4)],
    },
    EventSpec {
        label: "铜哨令",
        keyword: "铜哨令",
        query: "\"铜哨令\"下达的那一夜城里发生了什么",
        zero_words: &["下达", "那一夜", "发生", "什么"],
        occurrence: 23,
        occurrence_uses: 2,
        scene: &[
            "三更时分,铜哨令顺着风向传遍了全城,一声接着一声。",
            "他攥紧拳头想,铜哨令一响,就没有人能再装作不知道了。",
        ],
        recap_pool: &[
            "后来每逢变天,老人们就会提起那道铜哨令。",
            "说书人把铜哨令编进了段子,讲到紧要处总要停一停。",
            "军营里的新兵问起铜哨令,老兵们多半摇头不答。",
            "铜哨令三个字,渐渐成了城里人不愿提的旧事。",
        ],
        recaps: &[(37, 3), (51, 5), (79, 4), (103, 3)],
    },
    EventSpec {
        label: "北屿石门",
        keyword: "北屿石门",
        query: "\"北屿石门\"合拢的缘由是什么",
        zero_words: &["合拢", "缘由", "什么"],
        occurrence: 41,
        occurrence_uses: 1,
        scene: &["船绕过末段险滩,北屿石门在薄霭里显出了轮廓,静默地合着。"],
        recap_pool: &[
            "渔民们都说,北屿石门封死的那年,鱼汛来得格外早。",
            "账房里的旧簿子上还画着北屿石门,墨线已经淡了。",
            "有人赌咒发誓,说夜里听见过北屿石门后的钟声。",
            "北屿石门被封死的内情,官府的文卷里一个字也没有。",
        ],
        recaps: &[(55, 6), (67, 3), (92, 4), (130, 5), (158, 3)],
    },
    EventSpec {
        label: "空函案",
        keyword: "空函案",
        query: "\"空函案\"里的信到底寄给了谁",
        zero_words: &["到底", "寄给"],
        occurrence: 63,
        occurrence_uses: 2,
        scene: &[
            "头一封没有信笺的信寄来时,谁都以为空函案只是个玩笑。",
            "直到第七封,官府才认真查问,空函案这个称呼也随之传开。",
        ],
        recap_pool: &[
            "茶馆里讲空函案的人,总要把声音压得很低。",
            "空函案里那封没有信笺的信,至今没有人说得清落在了谁的手上。",
            "掌柜的翻遍卷宗,空函案的批语只有四个字:查无可查。",
            "多年后还有人仿照空函案寄白信,都被当成了笑话。",
        ],
        recaps: &[(75, 4), (97, 5), (112, 3), (150, 4)],
    },
    EventSpec {
        label: "沉银册",
        keyword: "沉银册",
        query: "\"沉银册\"最后落到了谁的手里",
        zero_words: &["最后", "落到", "手里"],
        occurrence: 86,
        occurrence_uses: 1,
        scene: &["箱底压着的沉银册边角发脆,翻页时簌簌作响。"],
        recap_pool: &[
            "关于沉银册的下落,街面上流传着三种说法。",
            "当铺朝奉一口咬定,沉银册早就随着船落进了深水。",
            "沉银册上记的究竟是账还是名,没有人敢往下猜。",
            "孩子们打赌,谁先找到沉银册,谁就是这条街的王。",
        ],
        recaps: &[(101, 3), (118, 6), (139, 4), (170, 5)],
    },
    EventSpec {
        label: "纸鸢阵",
        keyword: "纸鸢阵",
        query: "\"纸鸢阵\"是为了拦住什么",
        zero_words: &["为了", "拦住", "什么"],
        occurrence: 105,
        occurrence_uses: 2,
        scene: &[
            "天刚亮,几十只风筝同时升了上去,纸鸢阵在风里缓缓张开。",
            "他看懂了,纸鸢阵摆的不是戏,是一道挡人的墙。",
        ],
        recap_pool: &[
            "那年之后,每逢清明,孩子们还会自发摆一次纸鸢阵。",
            "老人们说,纸鸢阵拦的是不该进城的信。",
            "戏文里唱到纸鸢阵,台下总有人跟着掉眼泪。",
            "纸鸢阵的摆法传到今天,已经没有人记得全了。",
        ],
        recaps: &[(119, 4), (133, 3), (148, 5), (162, 6), (177, 3)],
    },
    EventSpec {
        label: "盲琴师",
        keyword: "盲琴师",
        query: "那位\"盲琴师\"演奏的曲子叫什么名字",
        zero_words: &["那位", "演奏", "曲子", "什么", "名字"],
        occurrence: 128,
        occurrence_uses: 1,
        scene: &["渡口的酒旗底下,盲琴师调好了弦,第一声就压住了满堂的喧哗。"],
        recap_pool: &[
            "后来城里再没有出过那样的琴声,盲琴师成了传说。",
            "有人专程去渡口等盲琴师,从春天等到落雪。",
            "盲琴师留下的调子没有名目,人们就叫它渡口调。",
            "说起盲琴师,掌柜的总会多添一盏茶。",
        ],
        recaps: &[(140, 5), (155, 3), (171, 4)],
    },
    EventSpec {
        label: "雾隐驿亭",
        keyword: "雾隐驿亭",
        query: "\"雾隐驿亭\"为什么只在起雾的日子出现",
        zero_words: &["为什么", "起雾", "日子", "出现"],
        occurrence: 149,
        occurrence_uses: 2,
        scene: &[
            "雾最浓的那天,雾隐驿亭毫无征兆地立在了官道尽头。",
            "赶脚的在檐外张望了半天,雾隐驿亭里静得能听见马嚼草的声音。",
        ],
        recap_pool: &[
            "关于雾隐驿亭,最多的一种说法是:它只收找不到路的信。",
            "老驿卒退休之后,逢人便讲雾隐驿亭里那盆从不熄的炭火。",
            "雾隐驿亭立在那儿的几天,官道上的湿气都比往年重。",
            "有人在雾散后折回去找雾隐驿亭,只看见一片平地。",
        ],
        recaps: &[(160, 4), (169, 3), (178, 5)],
    },
    EventSpec {
        label: "烬海图",
        keyword: "烬海图",
        query: "\"烬海图\"上标出的线路通向哪里",
        zero_words: &["标出", "线路", "通向", "哪里"],
        occurrence: 3,
        occurrence_uses: 1,
        scene: &["祖父留下的木匣里只有一张烬海图,边角有火燎过的焦痕。"],
        recap_pool: &[
            "烬海图里画着的航道,罗盘到了那里就会乱转。",
            "藏宝人的故事里,烬海图是唯一被反复提起的物件。",
            "有人照着烬海图走了一遭,回来后再也不肯出海。",
            "书商们私下议论,烬海图的原本早就被烧了,如今传的都是抄本。",
        ],
        recaps: &[
            (16, 3),
            (29, 5),
            (52, 4),
            (77, 6),
            (99, 3),
            (124, 5),
            (146, 4),
        ],
    },
];

// ---------------------------------------------------------------------------
// special queries (the T17/T18 behavior pins' material)
// ---------------------------------------------------------------------------

/// The structure-word flood query: pure furniture (the refrain verbatim
/// — every character sits in REFRAIN's vocabulary, self-checked), nothing
/// rare — under v1 it analyzed to ZERO candidates (the blind spot); under
/// T17's 恒门 it gates anyway (D-8.4: the all-structure escape hatch is
/// retired). FIXTURE SURGERY (first T17 env run): the v1 form carried the
/// tail ",说的是哪一件", and 说的 legitimately occurs in the P_FILLERS
/// padding (要说的话) at a MID-band df — it out-rared the saturated
/// furniture and stole the head, which is correct argmin behavior on an
/// impure query, not a mechanism bug. The pure form pins what the case
/// exists to pin.
const FLOOD_QUERY: &str = "夜里城里有人说起他的事";

/// The all-saturated-tie query: three QUOTED coined place names whose
/// chapter coverages are 72/84/96 (every probe saturates the cap 64), so
/// the head anchor is only decidable through T17's stage-two exact counts:
/// argmin = 乌篷巷 (72). Everything outside the quotes is single-character
/// (dropped by the candidate filter), so the probe set is exactly the tie.
const TIE_QUERY: &str = "从\"百步桥\"到\"乌篷巷\",要经过\"栖霞圩\"吗";

/// The long query: fourteen distinct multi-char words precede the one-shot
/// name 谢闻笛 (df = one chapter), which therefore sits past position 8 of
/// the candidate list — MAX_PROBES truncation means it is NEVER probed,
/// whatever the analyzer's exact segmentation (规范语义, D-8.4).
const LONG_QUERY: &str = "沈观澜连夜赶回旧城,跑遍南浦、津渡码头、青崖寺、回生药房,问遍船夫、更夫、药师、书商,没有人见过谢闻笛";

/// The multi-char words of LONG_QUERY ahead of the name (self-check
/// material: all present, all positioned before it).
const LONG_QUERY_PREWORDS: [&str; 14] = [
    "沈观澜",
    "连夜",
    "赶回",
    "旧城",
    "跑遍",
    "南浦",
    "津渡码头",
    "青崖寺",
    "回生药房",
    "问遍",
    "船夫",
    "更夫",
    "药师",
    "书商",
];

// ---------------------------------------------------------------------------
// the generator
// ---------------------------------------------------------------------------

/// The generated corpus: plain chapter texts (title line + paragraphs,
/// the self-check view) plus the two ingestable products.
struct Narrative {
    /// chapter texts, index i = section i+1 (title line first).
    chapters: Vec<String>,
    /// the .epub bytes (deterministic zip: fixed order, epoch timestamps).
    epub: Vec<u8>,
    /// the cross-source decoy document's text (ingested as .txt).
    decoy: String,
}

/// Render a template's slots: {P}/{S}/{L}/{O}/{T}.
fn render(tpl: &str, p: &str, s: &str, l: &str, o: &str, t: &str) -> String {
    tpl.replace("{P}", p)
        .replace("{S}", s)
        .replace("{L}", l)
        .replace("{O}", o)
        .replace("{T}", t)
}

/// Place one due entity: render a template from its pool into the
/// rotating body paragraph. `slot_letter` is the template placeholder the
/// name fills ({S}/{L}/{O}).
fn push_due(
    rng: &mut Rng,
    paras: &mut [Vec<String>],
    pool: &[&str],
    name: &str,
    slot_letter: char,
    slot: &mut usize,
) {
    let tpl = rng.pick_str(pool);
    let s = match slot_letter {
        'S' => render(tpl, PROTAGONIST, name, "", "", ""),
        'L' => render(tpl, PROTAGONIST, "", name, "", ""),
        _ => render(tpl, PROTAGONIST, "", "", name, ""),
    };
    paras[*slot].push(s);
    *slot = 1 + *slot % 3;
}

/// One chapter's plain text: `第N章 题词` then four paragraphs
/// (opening / body / body / refrain-carrying tail), assembled from the
/// policy queues and seeded picks — fully deterministic in c and the rng.
fn chapter_text(c: u32, rng: &mut Rng) -> String {
    // the separator is U+3000 (第N章　题词): an ASCII space flanked by CJK
    // is DROPPED by the html kind's whitespace rule (both-neighbors-CJK
    // collapse), the ideographic space survives — the committed book.epub
    // fixture spells its chapter titles the same way
    let title = format!("第{c}章\u{3000}{}", TITLE_WORDS[((c - 1) % 60) as usize]);
    let mut paras: Vec<Vec<String>> = vec![Vec::new(), Vec::new(), Vec::new(), Vec::new()];

    // the opening: protagonist, every chapter (the 主角 policy)
    paras[0].push(render(rng.pick_str(&OPENINGS), PROTAGONIST, "", "", "", ""));

    // the due queues (exactly-once placement below drains them)
    let due_s: Vec<&str> = SUPPORTING
        .iter()
        .filter(|(_, p, q)| (c + q).is_multiple_of(*p))
        .map(|(n, _, _)| *n)
        .collect();
    let due_l: Vec<&str> = PLACES
        .iter()
        .filter(|(_, p, q)| (c + q).is_multiple_of(*p))
        .map(|(n, _, _)| *n)
        .collect();
    let due_o: Vec<&str> = ORGS
        .iter()
        .filter(|(_, p, q)| (c + q).is_multiple_of(*p))
        .map(|(n, _, _)| *n)
        .collect();
    let due_t: Option<&str> = ONESHOTS
        .iter()
        .enumerate()
        .find(|(i, _)| (*i as u32 * 6) % 180 + 5 == c)
        .map(|(_, n)| *n);

    // due placements rotate paragraphs 1..3
    let mut slot = 1usize;
    for name in due_s {
        push_due(rng, &mut paras, &S_FILLERS, name, 'S', &mut slot);
    }
    for name in due_l {
        push_due(rng, &mut paras, &L_FILLERS, name, 'L', &mut slot);
    }
    for name in due_o {
        push_due(rng, &mut paras, &O_FILLERS, name, 'O', &mut slot);
    }
    if let Some(name) = due_t {
        paras[2].push(render(rng.pick_str(&T_FILLERS), "", "", "", "", name));
    }

    // planted-event inserts (occurrence scenes + recap sentences)
    for ev in EVENTS.iter() {
        if ev.occurrence == c {
            for i in 0..ev.occurrence_uses as usize {
                let target = if i == 0 { 1 } else { 2 };
                paras[target].push(ev.scene[i].to_string());
            }
        }
        for (i, (sec, uses)) in ev.recaps.iter().enumerate() {
            if *sec == c {
                for k in 0..*uses as usize {
                    let sentence = ev.recap_pool[k % ev.recap_pool.len()];
                    let target = 1 + (i + k) % 3;
                    paras[target].push(sentence.to_string());
                }
            }
        }
    }

    // tie-term inserts (one sentence, one occurrence, paragraph 2)
    for (i, (_, m, r)) in TIE_TERMS.iter().enumerate() {
        if c % m < *r {
            paras[2].push(TIE_SENTENCES[i].to_string());
        }
    }

    // padding to the paragraph targets, then the refrain (always last)
    const TARGETS: [usize; 4] = [2, 3, 3, 2];
    for (i, target) in TARGETS.iter().enumerate() {
        while paras[i].len() < *target {
            let pool: &[&str] = if rng.below(2) == 0 {
                &P_FILLERS
            } else {
                &ATMOSPHERE
            };
            paras[i].push(render(rng.pick_str(pool), PROTAGONIST, "", "", "", ""));
        }
    }
    paras[3].push(REFRAIN.to_string());

    let body = paras
        .iter()
        .map(|p| p.join(""))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{title}\n{body}")
}

/// Build the .epub: mimetype FIRST and stored (the OCF contract the epub
/// kind checks), container.xml, the OPF with the 180-item spine in natural
/// order, the chapters, and a non-spine nav document carrying the volume
/// structure (T18's nav-label chain material — D-9.2). Same hand-assembly
/// discipline as the epub kind's own builder (tests can't reach its
/// pub(crate) helper, so this is the integration-side twin).
fn build_epub(chapters: &[String]) -> Vec<u8> {
    use std::io::Write;
    let manifest = (0..chapters.len())
        .map(|i| {
            format!(
                "<item id=\"ch{i:03}\" href=\"ch{i:03}.xhtml\" \
                 media-type=\"application/xhtml+xml\"/>"
            )
        })
        .collect::<String>()
        + "<item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\"/>";
    let spine = (0..chapters.len())
        .map(|i| format!("<itemref idref=\"ch{i:03}\"/>"))
        .collect::<String>();
    let opf = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" \
         unique-identifier=\"id\"><metadata/><manifest>{manifest}</manifest>\
         <spine>{spine}</spine></package>"
    );
    let container_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <container version=\"1.0\" \
         xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\
         <rootfiles><rootfile full-path=\"OEBPS/content.opf\" \
         media-type=\"application/oebps-package+xml\"/></rootfiles></container>";
    // the nav: three volumes, sixty entries each (never on the spine)
    let mut nav_body = String::new();
    for v in 0..VOLUMES {
        nav_body.push_str(&format!("<li>卷{}<ol>", ['一', '二', '三'][v]));
        for k in 0..CHAPTERS_PER_VOLUME {
            let i = v * CHAPTERS_PER_VOLUME as usize + k as usize;
            let title_line = chapters[i].lines().next().unwrap_or_default();
            nav_body.push_str(&format!(
                "<li><a href=\"ch{i:03}.xhtml\">{title_line}</a></li>"
            ));
        }
        nav_body.push_str("</ol></li>");
    }
    let nav = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <html xmlns=\"http://www.w3.org/1999/xhtml\" \
         xmlns:epub=\"http://www.idpf.org/2007/ops\">\
         <body><nav epub:type=\"toc\"><ol>{nav_body}</ol></nav></body></html>"
    );

    let buf = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(buf);
    let opts: zip::write::SimpleFileOptions = Default::default();
    zip.start_file(
        "mimetype",
        opts.compression_method(zip::CompressionMethod::Stored),
    )
    .unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    for (name, bytes) in [
        ("META-INF/container.xml", container_xml.as_bytes()),
        ("OEBPS/content.opf", opf.as_bytes()),
    ] {
        zip.start_file(name, opts).unwrap();
        zip.write_all(bytes).unwrap();
    }
    for (i, ch) in chapters.iter().enumerate() {
        let (title, body) = ch.split_once('\n').unwrap_or((ch.as_str(), ""));
        let paragraphs = body
            .split('\n')
            .map(|p| format!("<p>{p}</p>"))
            .collect::<String>();
        let xhtml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <html xmlns=\"http://www.w3.org/1999/xhtml\">\
             <head><title>{title}</title></head>\
             <body><h1>{title}</h1>{paragraphs}</body></html>"
        );
        zip.start_file(format!("OEBPS/ch{i:03}.xhtml"), opts)
            .unwrap();
        zip.write_all(xhtml.as_bytes()).unwrap();
    }
    zip.start_file("OEBPS/nav.xhtml", opts).unwrap();
    zip.write_all(nav.as_bytes()).unwrap();
    zip.finish().unwrap().into_inner()
}

/// The cross-source decoy: a structure-word-dense "reader notes" sheet —
/// the furniture bigrams over and over, zero distinctive strings and zero
/// quarantined characters (it must not perturb any event/tie df; its job
/// is to FLOOD furniture queries from OUTSIDE the novel — the FM4 shape).
fn build_decoy() -> String {
    let mut lines: Vec<String> = vec!["夜读手记·其一".to_string()];
    lines.push("这一部书每一章的收尾都是同一句:夜里,城里有人说起他的事。".to_string());
    for bg in FURNITURE_BIGRAMS {
        lines.push(format!("关键词卡:{bg}。"));
        lines.push(format!("批注:{bg}指的是每一章都会回到的那个词。"));
        lines.push(format!("同义卡:{bg},还是{bg},写的仍是{bg}。"));
    }
    for i in 0..16 {
        lines.push(format!("复述练习其{i}:夜里,城里有人说起他的事。"));
    }
    lines.push("备忘:先查夜里,再查城里,再查有人,再查说起。".to_string());
    // FIXTURE SURGERY (first T17 env run): the v1 line said 末尾都会出现 —
    // and 出现 is event E9's zero-word, which the T16 zero-word hygiene
    // checked against the NOVEL ONLY: the decoy leaked it, df(decoy chunks)
    // < df(雾隐驿亭), and the E9 head anchor went to 出现. The rephrased
    // line keeps the statistic's meaning without the leak; the self-check
    // below now scans the decoy too (the gap this exposed).
    lines.push("统计:这句在正文里每一章的末尾都有,没有例外。".to_string());
    lines.push("结论:夜里与城里同章,有人与说起同句,检索时四者等价。".to_string());
    lines.join("\n")
}

fn generate(seed: u64) -> Narrative {
    let mut rng = Rng(seed);
    let chapters: Vec<String> = (1..=CHAPTERS).map(|c| chapter_text(c, &mut rng)).collect();
    let epub = build_epub(&chapters);
    Narrative {
        chapters,
        epub,
        decoy: build_decoy(),
    }
}

// ---------------------------------------------------------------------------
// text bookkeeping helpers (the self-checks count the FINAL text — the
// generated product is the single source of truth, not the policy tables)
// ---------------------------------------------------------------------------

fn occurrences_in_chapter(n: &Narrative, section: u32, term: &str) -> usize {
    n.chapters[(section - 1) as usize].matches(term).count()
}

fn chapters_containing(n: &Narrative, term: &str) -> Vec<u32> {
    (1..=CHAPTERS)
        .filter(|&c| occurrences_in_chapter(n, c, term) > 0)
        .collect()
}

fn whole_corpus(n: &Narrative) -> String {
    n.chapters.join("\n")
}

/// Read one deflated entry back out of the built epub (the nav document
/// never rides the spine, so this is its only reader-side view).
fn zip_entry(bytes: &[u8], name: &str) -> String {
    let cursor = std::io::Cursor::new(bytes.to_vec());
    let mut zip = zip::ZipArchive::new(cursor).expect("reopen the built epub");
    let mut s = String::new();
    zip.by_name(name)
        .unwrap_or_else(|e| panic!("entry {name}: {e}"))
        .read_to_string(&mut s)
        .unwrap();
    s
}

/// The full distinctive vocabulary (for the pairwise non-substring check).
fn distinctive_terms() -> Vec<&'static str> {
    let mut v = vec![PROTAGONIST];
    v.extend(SUPPORTING.iter().map(|(n, _, _)| *n));
    v.extend(PLACES.iter().map(|(n, _, _)| *n));
    v.extend(ORGS.iter().map(|(n, _, _)| *n));
    v.extend(ONESHOTS.iter().copied());
    v.extend(EVENTS.iter().map(|e| e.keyword));
    v.extend(TIE_TERMS.iter().map(|(t, _, _)| *t));
    v
}

/// All CJK characters of the CJK event keywords + tie terms — the
/// quarantine set (checked char-by-char against the corpus).
fn quarantined_chars() -> Vec<char> {
    let mut s = String::new();
    for e in EVENTS.iter() {
        s.push_str(e.keyword);
    }
    for (t, _, _) in TIE_TERMS.iter() {
        s.push_str(t);
    }
    s.chars().collect()
}

// ---------------------------------------------------------------------------
// immediately-green gates: determinism, structure, density, hygiene
// ---------------------------------------------------------------------------

/// T16 determinism gate: same seed ⇒ byte-identical products (epub bytes,
/// decoy, chapter texts); a different seed ⇒ different bytes (the seed is
/// load-bearing, not decorative). No DefaultHasher, no clock, no map
/// iteration anywhere in the generator (F-12).
#[test]
fn narrative_generator_same_seed_same_bytes() {
    let a = generate(SEED);
    let b = generate(SEED);
    assert_eq!(
        a.epub, b.epub,
        "same seed must rebuild the epub byte-for-byte"
    );
    assert_eq!(a.decoy, b.decoy);
    assert_eq!(a.chapters, b.chapters);
    let c = generate(SEED ^ 1);
    assert_ne!(a.epub, c.epub, "a different seed must move the bytes");
    assert_ne!(a.chapters, c.chapters);
    assert!(
        a.epub.len() > 50_000 && a.epub.len() < 500_000,
        "corpus size band: {} bytes",
        a.epub.len()
    );
}

/// T16 structure gate: 180 chapters, 第N章 titles, the volume map in the
/// nav, the protagonist in every chapter, exact entity bands (period
/// policy honored exactly-once), one-shots at exactly one chapter, the
/// furniture refrain everywhere, tie-term coverages at the designed
/// 72/84/96, and the two hygienes (pairwise non-substring vocabulary;
/// quarantined characters only inside their keyword's instances).
#[test]
fn narrative_corpus_structure_self_check() {
    let n = generate(SEED);
    assert_eq!(n.chapters.len(), CHAPTERS as usize);

    // titles: 第N章 form, N == the section axis, word from the pool
    for (i, ch) in n.chapters.iter().enumerate() {
        let want = format!("第{}章\u{3000}{}", i + 1, TITLE_WORDS[i % 60]);
        let head: String = ch.chars().take(12).collect();
        assert!(ch.starts_with(&want), "chapter {} title: {head:?}", i + 1);
        assert!(
            !ch.contains('<') && !ch.contains('&'),
            "xhtml-unsafe char in chapter {}",
            i + 1
        );
    }

    // the volume map rides the nav document (never on the spine — the
    // entry is read back out of the zip, it is not on any spine)
    let nav = zip_entry(&n.epub, "OEBPS/nav.xhtml");
    assert_eq!(nav.matches("<a href=\"ch").count(), CHAPTERS as usize);
    for v in ["卷一", "卷二", "卷三"] {
        assert!(nav.contains(v), "nav must nest {v}");
    }

    // 主角: every chapter
    for c in 1..=CHAPTERS {
        assert!(
            occurrences_in_chapter(&n, c, PROTAGONIST) >= 1,
            "protagonist missing from chapter {c}"
        );
    }

    // 配角/地名/组织: exactly once in each due chapter, zero elsewhere
    let mut policy: Vec<(&str, u32, u32)> = Vec::new();
    policy.extend(SUPPORTING);
    policy.extend(PLACES);
    policy.extend(ORGS);
    for &(name, period, phase) in &policy {
        let due: Vec<u32> = (1..=CHAPTERS)
            .filter(|c| (c + phase) % period == 0)
            .collect();
        let got = chapters_containing(&n, name);
        assert_eq!(
            got, due,
            "{name}: policy chapters and text chapters must agree exactly"
        );
        for c in &due {
            assert_eq!(
                occurrences_in_chapter(&n, *c, name),
                1,
                "{name} must appear exactly once in chapter {c}"
            );
        }
    }

    // 一次性实体: exactly one chapter, one mention, 30 distinct chapters
    let mut oneshot_chapters: Vec<u32> = Vec::new();
    for name in ONESHOTS {
        let got = chapters_containing(&n, name);
        assert_eq!(
            got.len(),
            1,
            "{name} must live in exactly one chapter: {got:?}"
        );
        assert_eq!(
            occurrences_in_chapter(&n, got[0], name),
            1,
            "{name} exactly once"
        );
        oneshot_chapters.push(got[0]);
    }
    assert_eq!(
        oneshot_chapters.len(),
        oneshot_chapters
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        "one-shots must occupy 30 DISTINCT chapters"
    );

    // the furniture refrain: every chapter, all four bigrams
    for c in 1..=CHAPTERS {
        assert!(
            n.chapters[(c - 1) as usize].contains(REFRAIN),
            "refrain missing from chapter {c}"
        );
        for bg in FURNITURE_BIGRAMS {
            assert!(
                occurrences_in_chapter(&n, c, bg) >= 1,
                "{bg} in chapter {c}"
            );
        }
    }

    // tie terms: the designed coverage, the residue rule, one occurrence
    for &(term, m, r) in TIE_TERMS.iter() {
        let due: Vec<u32> = (1..=CHAPTERS).filter(|c| c % m < r).collect();
        let got = chapters_containing(&n, term);
        assert_eq!(got, due, "{term}: residue-rule chapters must match text");
        for c in &got {
            assert_eq!(
                occurrences_in_chapter(&n, *c, term),
                1,
                "{term} once in {c}"
            );
        }
        assert!(
            got.len() as u64 >= 64,
            "{term} coverage {} must saturate the probe cap (>= 64 chapters)",
            got.len()
        );
    }
    // the designed argmin order: 乌篷巷(72) < 百步桥(84) < 栖霞圩(96)
    let cov = |t: &str| chapters_containing(&n, t).len();
    assert!(
        cov("乌篷巷") < cov("百步桥") && cov("百步桥") < cov("栖霞圩"),
        "tie coverages must stay ordered: {} / {} / {}",
        cov("乌篷巷"),
        cov("百步桥"),
        cov("栖霞圩")
    );

    // hygiene 1: pairwise non-substring distinctive vocabulary
    let vocab = distinctive_terms();
    for i in 0..vocab.len() {
        for j in (i + 1)..vocab.len() {
            let (a, b) = (vocab[i], vocab[j]);
            assert!(
                !a.contains(b) && !b.contains(a),
                "distinctive terms must not be substrings of each other: {a:?} vs {b:?}"
            );
        }
    }

    // hygiene 2: quarantined characters live only inside their keyword's
    // OWN chapters (occurrence + recaps; there partial words like 琴声 in a
    // recap are harmless — those chunks already carry the keyword). This
    // is what makes a quoted multi-token keyword probe as its own event's
    // chunk count under lance's Or-operator MatchQuery.
    for e in EVENTS.iter().filter(|e| !e.keyword.is_ascii()) {
        let own: std::collections::BTreeSet<u32> = std::iter::once(e.occurrence)
            .chain(e.recaps.iter().map(|(s, _)| *s))
            .collect();
        for c in 1..=CHAPTERS {
            if own.contains(&c) {
                continue;
            }
            for ch in e.keyword.chars().filter(|c| !c.is_ascii()) {
                assert_eq!(
                    n.chapters[(c - 1) as usize].matches(ch).count(),
                    0,
                    "quarantine breach: {ch:?} of keyword {:?} appears in chapter {c} \
                     (outside the event's own chapters)",
                    e.keyword
                );
            }
        }
    }
    // tie terms stay STRICT (their df must equal the residue-rule coverage
    // exactly — the two-stage argmin rides it): no character of a tie term
    // may appear outside the term's instances anywhere
    let corpus = whole_corpus(&n);
    for (t, _, _) in TIE_TERMS.iter() {
        let stripped = corpus.replace(*t, "");
        for ch in t.chars() {
            assert_eq!(
                stripped.matches(ch).count(),
                0,
                "quarantine breach: {ch:?} of tie term {t:?} appears outside the term"
            );
        }
    }
}

/// T16 density-difference gate (the FM2 model, per event): exactly ONE
/// occurrence chapter (keyword 1–2 times), 3–8 recap chapters at 3–6 uses
/// each, every recap section strictly greater than the occurrence section,
/// and zero keyword occurrences in every other chapter.
#[test]
fn narrative_event_density_self_check() {
    let n = generate(SEED);
    let mut occurrences: Vec<u32> = Vec::new();
    for e in EVENTS.iter() {
        // the designed count of chapter c (0 when no slot designed)
        let designed = |c: u32| -> usize {
            if c == e.occurrence {
                e.occurrence_uses as usize
            } else {
                e.recaps
                    .iter()
                    .find(|(s, _)| *s == c)
                    .map(|(_, u)| *u as usize)
                    .unwrap_or(0)
            }
        };
        // band shape of the table itself
        assert!(
            (1..=2).contains(&e.occurrence_uses),
            "{}: occurrence uses out of band",
            e.label
        );
        assert!(
            (3..=8).contains(&e.recaps.len()),
            "{}: recap count out of band",
            e.label
        );
        for &(sec, uses) in e.recaps {
            assert!(
                (3..=6).contains(&uses),
                "{}: recap {sec} uses {uses} out of band",
                e.label
            );
            assert!(
                sec > e.occurrence,
                "{}: recap {sec} must be AFTER the occurrence chapter {}",
                e.label,
                e.occurrence
            );
        }
        // the text matches the table everywhere, and exactly one chapter
        // sits in the 1..=2 band (出现章恰一)
        let mut low = 0;
        for c in 1..=CHAPTERS {
            let k = occurrences_in_chapter(&n, c, e.keyword);
            assert_eq!(k, designed(c), "{}: chapter {c} count", e.label);
            if (1..=2).contains(&k) {
                low += 1;
            }
        }
        assert_eq!(low, 1, "{}: exactly one chapter in the 1-2 band", e.label);
        occurrences.push(e.occurrence);
    }
    // occurrence chapters are pairwise distinct (one event per chapter)
    occurrences.sort_unstable();
    occurrences.dedup();
    assert_eq!(
        occurrences.len(),
        EVENTS.len(),
        "each event needs its own occurrence chapter"
    );
}

/// T16 fixture gate: the query fixtures reference the planted truth. Every
/// event query carries its keyword exactly once (quoted for CJK), contains
/// no foreign distinctive term, and its zero-words have ZERO corpus
/// occurrences (the argmin hygiene); the flood query is pure furniture;
/// the tie query's non-quoted remainder is exactly the single-char frame;
/// the long query's fourteen prewords all precede the one-shot name.
#[test]
fn narrative_query_fixture_self_check() {
    let n = generate(SEED);
    let corpus = whole_corpus(&n);

    for e in EVENTS.iter() {
        // keyword exactly once in the query
        assert_eq!(
            e.query.matches(e.keyword).count(),
            1,
            "{}: query must carry its keyword exactly once",
            e.label
        );
        // CJK keywords are quoted (the D-8.3 quoted-span candidate)
        if !e.keyword.is_ascii() {
            assert!(
                e.query.contains(&format!("\"{}\"", e.keyword)),
                "{}: CJK keyword must be quoted in the query",
                e.label
            );
        }
        // no foreign distinctive term rides the query (the protagonist is
        // allowed — naming him in a query is natural and his df is the
        // corpus's ceiling, never the argmin)
        for term in distinctive_terms() {
            if term != e.keyword && term != PROTAGONIST {
                assert!(
                    !e.query.contains(term),
                    "{}: query leaks foreign term {term:?}",
                    e.label
                );
            }
        }
        // zero-words: zero occurrences in the NOVEL and in the DECOY (each
        // is a stopgap word the corpus never uses — this keeps the quoted
        // keyword the argmin; the decoy half of the check was added after
        // the first T17 env run caught the decoy leaking E9's 出现 at
        // decoy-chunk df, stealing that event's head anchor)
        for w in e.zero_words {
            assert_eq!(
                corpus.matches(w).count(),
                0,
                "{}: zero-word {w:?} appears in the novel",
                e.label
            );
            assert!(
                !n.decoy.contains(w),
                "{}: zero-word {w:?} appears in the decoy (its small df would \
                 out-rare the keyword and steal the head anchor)",
                e.label
            );
        }
        // ground truth shape: occurrence in range, recaps after, 3..8 of
        // them (the density gate re-checks the text side)
        assert!((1..=CHAPTERS).contains(&e.occurrence));
        assert!((3..=8).contains(&e.recaps.len()));
        for (sec, _) in e.recaps {
            assert!(sec > &e.occurrence && *sec <= CHAPTERS);
        }
    }

    // flood query: pure furniture — all four bigrams, and EVERY character
    // sits in the refrain's vocabulary (no stray word can ride along and
    // out-rare the saturated furniture — the v1 form's 说的 lesson)
    for bg in FURNITURE_BIGRAMS {
        assert!(FLOOD_QUERY.contains(bg), "flood query must ride {bg}");
    }
    assert!(
        FLOOD_QUERY.chars().all(|c| REFRAIN.contains(c)),
        "flood query must be pure furniture vocabulary"
    );
    // tie query: three quoted terms; the unquoted remainder is exactly the
    // single-char frame (so the probe set is exactly the tie)
    for (t, _, _) in TIE_TERMS.iter() {
        assert!(
            TIE_QUERY.contains(&format!("\"{t}\"")),
            "tie query must quote {t}"
        );
    }
    let stripped = TIE_QUERY
        .replace(&format!("\"{}\"", "百步桥"), "")
        .replace(&format!("\"{}\"", "乌篷巷"), "")
        .replace(&format!("\"{}\"", "栖霞圩"), "");
    assert_eq!(stripped, "从到,要经过吗", "tie query frame: {stripped:?}");

    // long query: every preword present and ahead of the name; the name
    // is a one-shot (df = one chapter by the structure gate)
    let name_pos = LONG_QUERY
        .find("谢闻笛")
        .expect("long query carries the name");
    for w in LONG_QUERY_PREWORDS {
        let pos = LONG_QUERY
            .find(w)
            .unwrap_or_else(|| panic!("long query preword {w} missing"));
        assert!(pos < name_pos, "preword {w} must precede the name");
    }
    let name_chapters = chapters_containing(&n, "谢闻笛");
    assert_eq!(
        name_chapters.len(),
        1,
        "the long query's name is a one-shot"
    );
}

/// T16 round-trip gate through the REAL production faces: the generated
/// epub unpacks into 180 spine-ordered children, each child extracts
/// through the real html kind with title, refrain and EXACT keyword counts
/// intact, and the token-window chunker never drops a planted mention
/// (every event chapter's chunks still carry the keyword — the "可词法
/// 检索" claim proven at the layer the FTS index actually reads).
#[test]
fn narrative_epub_round_trips_real_extractors() {
    let n = generate(SEED);
    // EPUB_MIME is pub(crate) in the kind; the canonical mime is the OCF
    // constant this builder wrote into the mimetype entry
    let children = EpubExtractor
        .unpack(&n.epub, "application/epub+zip")
        .expect("generated epub must unpack")
        .expect("container verdict");
    assert_eq!(
        children.len(),
        CHAPTERS as usize,
        "one child per spine chapter"
    );
    for (i, child) in children.iter().enumerate() {
        assert_eq!(
            child.name,
            format!("OEBPS/ch{i:03}.xhtml"),
            "spine == natural order"
        );
        assert_eq!(child.mime, "text/html", "the html routing mime");
        let parts = HtmlExtractor
            .extract(&child.bytes, "text/html")
            .unwrap_or_else(|e| panic!("chapter {} must extract: {e}", i + 1));
        assert_eq!(parts.len(), 1);
        let text = &parts[0].text;
        let want_title = format!("第{}章\u{3000}", i + 1);
        let head: String = text.chars().take(12).collect();
        assert!(text.contains(&want_title), "h1 title survives: {head:?}");
        assert!(
            text.contains(PROTAGONIST),
            "protagonist survives chapter {}",
            i + 1
        );
        assert!(text.contains(REFRAIN), "refrain survives chapter {}", i + 1);

        // exact keyword counts survive extraction, and windowing keeps
        // every mention reachable at chunk level
        let section = i as u32 + 1;
        let wants: Vec<(&EventSpec, usize)> = EVENTS
            .iter()
            .map(|e| {
                let want = if e.occurrence == section {
                    e.occurrence_uses as usize
                } else {
                    e.recaps
                        .iter()
                        .find(|(s, _)| *s == section)
                        .map(|(_, u)| *u as usize)
                        .unwrap_or(0)
                };
                (e, want)
            })
            .collect();
        for (e, want) in &wants {
            assert_eq!(
                text.matches(e.keyword).count(),
                *want,
                "{} in chapter {}: extraction kept the designed count",
                e.label,
                section
            );
        }
        if wants.iter().any(|(_, w)| *w > 0) {
            let chunker = TokenWindowChunker::new(512, 64).unwrap();
            let chunks = chunker.chunk(std::slice::from_ref(&parts[0])).unwrap();
            for (e, _) in wants.iter().filter(|(_, w)| *w > 0) {
                assert!(
                    chunks.iter().any(|c| c.text.contains(e.keyword)),
                    "{} chapter {}: some chunk must carry the keyword",
                    e.label,
                    section
                );
            }
        }
    }
    // determinism against the real faces
    let again = EpubExtractor
        .unpack(&n.epub, "application/epub+zip")
        .unwrap()
        .unwrap();
    assert_eq!(children, again);
}

/// T16 decoy gate: structure-dense, distinctive-free. The decoy must flood
/// furniture queries from outside the novel (FM4) WITHOUT perturbing any
/// event/tie/df (no distinctive terms, no quarantined characters at all).
#[test]
fn narrative_decoy_is_structure_dense_but_distinctive_free() {
    let n = generate(SEED);
    for bg in FURNITURE_BIGRAMS {
        assert!(
            n.decoy.matches(bg).count() >= 20,
            "decoy must be dense in {bg}: {}",
            n.decoy.matches(bg).count()
        );
    }
    for term in distinctive_terms() {
        assert!(
            !n.decoy.contains(term),
            "decoy leaks distinctive term {term:?} (would perturb its df)"
        );
    }
    for ch in quarantined_chars() {
        assert!(
            !n.decoy.contains(ch),
            "decoy leaks quarantined char {ch:?} (would perturb a keyword's Or-probe)"
        );
    }
}

/// The candidate shape under T17's analyze()-based extractor (v3): the
/// QUOTED CJK keyword is candidate #1 of every event query (any script,
/// priority order — D-8.3), the Latin identifier event leads with its
/// analyzer-lowercased token, the special queries analyze NON-empty (the
/// v1 F1 blind spot is gone), and the long query's one-shot name 谢闻笛
/// never becomes a candidate (icu dictionary-segments the coined name
/// into single chars, and the pre-name words fill MAX_PROBES first — the
/// 规范语义 pin). The chapter-level argmin pre-check: every NON-keyword
/// candidate covers at least as many chapters as the keyword (the quoted
/// keyword is tied-rarest before any probe runs — the indexed gate below
/// asserts the real chunk-level head).
#[test]
fn narrative_candidate_shape_under_t17_analyzer() {
    use vectorfs_server::lexical::{LanceFts, anchor_candidates};
    use vectorfs_server::pipeline::config::FtsTokenizer;

    let n = generate(SEED);
    let fts = LanceFts;
    for e in EVENTS.iter() {
        let cands = anchor_candidates(&fts, FtsTokenizer::Icu, e.query);
        assert!(
            !cands.is_empty(),
            "{}: T17 analyzes the query to candidates (F1 fixed)",
            e.label
        );
        let head = if e.label == "MeridianVow" {
            // the Latin identifier rides the analyzer term (lowercased —
            // the index's own token; the F1 guard property is unchanged:
            // the event gates on exactly its identifier)
            "meridianvow"
        } else {
            e.keyword
        };
        assert_eq!(
            cands[0], head,
            "{}: the (quoted) keyword keeps probe priority",
            e.label
        );
        // the argmin pre-check at chapter level: every OTHER candidate is
        // either ABSENT (0 chapters and not in the decoy ⇒ df=0, never
        // gates — the zero-words) or covers at least as many chapters as
        // the keyword (subwords tie — same chapters by quarantine;
        // 只在-class strays cover 30 chapters, never fewer than the
        // event's own). A candidate present in the 1..kw_chapters band —
        // novel OR decoy — would out-rare the keyword and steal the head;
        // that is the failure this guards.
        let kw_chapters = chapters_containing(&n, e.keyword).len();
        for c in cands.iter().skip(1) {
            let got = chapters_containing(&n, c).len();
            assert!(
                (got == 0 && !n.decoy.contains(c.as_str())) || got >= kw_chapters,
                "{}: candidate {c:?} covers {got} chapters (decoy: {}) vs keyword's \
                 {kw_chapters} — present-but-rarer would steal the head-anchor argmin",
                e.label,
                n.decoy.contains(c.as_str())
            );
        }
    }

    // the special queries: non-empty under v3 (v1 pinned them EMPTY — the
    // record this test replaced)
    for q in [FLOOD_QUERY, TIE_QUERY, LONG_QUERY] {
        assert!(
            !anchor_candidates(&fts, FtsTokenizer::Icu, q).is_empty(),
            "v3 analyzes {q:?} to candidates"
        );
    }
    // the tie query's probe set: the three quoted terms LEAD (priority
    // order), ahead of their dictionary subwords
    let tie = anchor_candidates(&fts, FtsTokenizer::Icu, TIE_QUERY);
    assert_eq!(&tie[..3], &["百步桥", "乌篷巷", "栖霞圩"]);
    // the long query: exactly MAX_PROBES candidates, and the one-shot
    // name 谢闻笛 is never among them (the truncation 规范语义)
    let long = anchor_candidates(&fts, FtsTokenizer::Icu, LONG_QUERY);
    assert_eq!(
        long.len(),
        vectorfs_server::lexical::MAX_PROBES,
        "the long query fills the probe set exactly"
    );
    assert!(
        long.iter().all(|c| !c.contains("谢闻笛")),
        "the 9th+-position name must never be probed: {long:?}"
    );
}

// ---------------------------------------------------------------------------
// env-gated full-chain harness (flagship_recall discipline: no env ⇒
// panic with the fix, never a silent skip)
// ---------------------------------------------------------------------------

const AK: &str = "VFNARRATIVE000001";
const SK: &str = "vf-narrative-corpus-secret";
const NOVEL_KEY: &str = "novel/mist-sea-chronicle.epub";
const DECOY_KEY: &str = "novel/reader-notes-decoy.txt";

fn models_dir() -> PathBuf {
    match std::env::var("VF_NARRATIVE_MODELS_DIR") {
        Ok(m) => PathBuf::from(m),
        Err(_) => panic!(
            "VF_NARRATIVE_MODELS_DIR must point at a models dir holding \
             gte-multilingual-base (the flagship acceptance cache works) — this gate \
             needs the real embedder and refuses to skip silently (see module doc)"
        ),
    }
}

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
        "the narrative INDEXED gate needs the real ONNX Runtime at {} — run \
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

struct Daemon {
    child: Child,
    /// The serve log's path (keep for on-failure inspection; printed at
    /// harness startup so a hanging gate tells the operator where to look).
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
        let d = Daemon { child, log };
        eprintln!(
            "serve pid {} on port {} (log {})",
            d.child.id(),
            port,
            d.log.display()
        );
        d
    }
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
    panic!("serve never became healthy on port {port} (warm models dir expected — see module doc)");
}

/// Isolated XDG_CONFIG_HOME so a developer's real client config cannot leak
/// credentials into the test daemon (the cli_smoke discipline).
fn empty_config_home() -> PathBuf {
    let d = std::env::temp_dir().join(format!("vf-narrative-no-config-{}", std::process::id()));
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

fn config_toml(models_dir: &Path) -> String {
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

[extractor.book]
kind = "epub"

[extractor.web]
kind = "html"

[extractor.plain]
kind = "txt"

[ingest.docs]
extract = {{ epub = "book", html = "web", txt = "plain" }}
chunk = {{ kind = "token-window", size = 512, overlap = 64 }}
embedders = ["text"]
merge = "rrf"
fts = true
scan_interval_secs = 2
"#,
        models = models_dir.display()
    )
}

/// A standing, INDEXED, FTS-optimized narrative corpus: writes the
/// generated epub + decoy, serves a real daemon (real gte model), puts
/// both sources to INDEXED, runs `vectorfs optimize` (the single-writer
/// FTS verb, daemon down), and re-serves for query work. With
/// VF_NARRATIVE_KEEP_DIR set the data dir persists at that path (T17's
/// calibration reuse); otherwise it is a tempdir.
struct IndexedNarrative {
    _scratch: tempfile::TempDir,
    data_dir: PathBuf,
    port: u16,
    /// Held (never read) so its Drop kills the daemon at test end.
    _daemon: Daemon,
}

fn stand_indexed_narrative(tag: &str) -> IndexedNarrative {
    let models = models_dir();
    let corpus = generate(SEED);
    let scratch = tempfile::tempdir().expect("scratch dir");
    let (data_dir, persistent) = match std::env::var("VF_NARRATIVE_KEEP_DIR") {
        Ok(k) => {
            let p = PathBuf::from(&k);
            std::fs::create_dir_all(&p).unwrap();
            // the daemon's log lands beside the data dir — that parent
            // must exist too
            std::fs::create_dir_all(p.parent().unwrap_or(Path::new("."))).unwrap();
            (p, true)
        }
        Err(_) => (scratch.path().join("data"), false),
    };
    std::fs::create_dir_all(data_dir.join(".vectorfs")).unwrap();
    std::fs::write(
        data_dir.join(".vectorfs/vectorfs.toml"),
        config_toml(&models),
    )
    .unwrap();
    // the corpus files (also dropped beside a persistent data dir for
    // out-of-band reuse)
    let novel_path = scratch.path().join("mist-sea-chronicle.epub");
    let decoy_path = scratch.path().join("reader-notes-decoy.txt");
    std::fs::write(&novel_path, &corpus.epub).unwrap();
    std::fs::write(&decoy_path, &corpus.decoy).unwrap();
    if persistent {
        let _ = std::fs::copy(
            &novel_path,
            data_dir.with_file_name("mist-sea-chronicle.epub"),
        );
        let _ = std::fs::copy(
            &decoy_path,
            data_dir.with_file_name("reader-notes-decoy.txt"),
        );
    }

    let port = free_port();
    let mut daemon = Daemon::spawn(&data_dir, port, &format!("{tag}-serve1.log"));
    wait_healthy(port, Instant::now() + Duration::from_secs(900));

    for (path, key) in [(&novel_path, NOVEL_KEY), (&decoy_path, DECOY_KEY)] {
        eprintln!("put {} as {key}", path.display());
        let o = vf(
            port,
            &[
                "put",
                path.to_str().unwrap(),
                "--bucket",
                "docs",
                "--key",
                key,
                "--timeout",
                "1500",
            ],
        );
        assert!(
            o.status.success() && out(&o).contains("INDEXED"),
            "put {key} did not reach INDEXED\nstdout:\n{}\nstderr:\n{}",
            out(&o),
            err(&o)
        );
    }

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
    let daemon = Daemon::spawn(&data_dir, port, &format!("{tag}-serve2.log"));
    wait_healthy(port, Instant::now() + Duration::from_secs(120));

    IndexedNarrative {
        _scratch: scratch,
        data_dir,
        port,
        _daemon: daemon,
    }
}

/// keyword-mode search through the CLI --json face (the same face the
/// flagship acceptance drives).
fn search_json(x: &IndexedNarrative, query: &str, top_k: u32) -> serde_json::Value {
    let o = vf(
        x.port,
        &[
            "search",
            query,
            "docs",
            "--mode",
            "keyword",
            "--top-k",
            &top_k.to_string(),
            "--json",
        ],
    );
    assert!(
        o.status.success(),
        "search {query:?} failed: {}{}",
        out(&o),
        err(&o)
    );
    serde_json::from_str(&out(&o))
        .unwrap_or_else(|e| panic!("search {query:?} json: {e}\n{}", out(&o)))
}

/// A search response answered with at least one hit.
fn has_hits(v: &serde_json::Value) -> bool {
    v["hits"].as_array().is_some_and(|a| !a.is_empty())
}

/// The derived index under `<data>/docs-vf/indexes/` (the r1_calibration
/// discovery) — reads are safe while the daemon holds the app-level flock.
fn first_index(data_dir: &Path) -> String {
    let indexes_dir = data_dir.join("docs-vf").join("indexes");
    for entry in std::fs::read_dir(&indexes_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", indexes_dir.display()))
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().join("index.json").is_file() {
            return name;
        }
    }
    panic!(
        "no derived index under {} — ingest + optimize first",
        indexes_dir.display()
    );
}

// ---------------------------------------------------------------------------
// the env-gated full-chain INDEXED gate (today-true assertions only)
// ---------------------------------------------------------------------------

/// The T16 full-chain gate: the generated corpus through a REAL daemon
/// with a REAL model — both sources INDEXED, the FTS index built, then the
/// structural facts pinned at the index level: the section axis is the
/// chapter axis (gapless 1..=180 over the novel, absent on the decoy),
/// every event's keyword lands in EXACTLY its occurrence + recap chapters
/// (the density model surviving the real pipeline), the tie terms order
/// as designed at chunk level, furniture saturates the probe cap, and the
/// occurrence scene is reachable through the ungated keyword face (the
/// A-R8 recall half, today-true) while recaps flood above it (the FM2
/// observation, printed not asserted — 排序病不治, design §5.5).
#[test]
#[ignore = "needs a real embedder model; set VF_NARRATIVE_MODELS_DIR (see module doc)"]
fn narrative_indexed_full_chain_gate() {
    let x = stand_indexed_narrative("gate");
    let storage = Storage::file(&x.data_dir);
    let ix = first_index(&x.data_dir);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio rt");

    // ---- the full-row scan: sections are the chapter axis ----
    let rows = rt
        .block_on(storage.scan_vector_metadata("docs", &ix))
        .expect("scan rows");
    let mut novel_sections: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let mut decoy_rows = 0usize;
    for (_, m) in &rows {
        let m = m.as_ref().expect("metadata present");
        if m["x-vf-src-name"].as_str() == Some(NOVEL_KEY) {
            novel_sections.insert(
                m["x-vf-section"]
                    .as_u64()
                    .expect("novel rows carry sections"),
            );
        } else if m["x-vf-src-name"].as_str() == Some(DECOY_KEY) {
            decoy_rows += 1;
            assert!(
                m.get("x-vf-section").is_none(),
                "a flat txt source carries no section"
            );
        }
    }
    let want: std::collections::BTreeSet<u64> = (1..=CHAPTERS as u64).collect();
    assert_eq!(
        novel_sections, want,
        "the section axis is the chapter axis, gapless"
    );
    assert!(decoy_rows >= 1, "the decoy source indexed");

    // ---- every event's keyword: EXACTLY the planted chapters ----
    for e in EVENTS.iter() {
        let mut got: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        for (_, m) in &rows {
            let m = m.as_ref().unwrap();
            if m["x-vf-src-name"].as_str() == Some(NOVEL_KEY)
                && m["x-vf-chunk-text"]
                    .as_str()
                    .is_some_and(|t| t.contains(e.keyword))
            {
                got.insert(m["x-vf-section"].as_u64().unwrap());
            }
        }
        let want: std::collections::BTreeSet<u64> = std::iter::once(e.occurrence as u64)
            .chain(e.recaps.iter().map(|(s, _)| *s as u64))
            .collect();
        assert_eq!(
            got, want,
            "{}: the density model survived the real pipeline",
            e.label
        );
    }

    // ---- the df probes: saturation and the two-stage raw material ----
    let kernel = vectorfs_server::lexical::kernel();
    let probe = |term: &str| {
        rt.block_on(kernel.probe_df(&storage, "docs", &ix, term))
            .unwrap_or_else(|e| panic!("probe {term:?}: {e}"))
    };
    let deep = |term: &str| {
        rt.block_on(storage.keyword_search("docs", &ix, term, 2000, None))
            .unwrap()
            .len() as u64
    };
    for bg in FURNITURE_BIGRAMS {
        assert_eq!(
            probe(bg),
            vectorfs_server::lexical::PROBE_DF_LIMIT as u64,
            "furniture {bg} must saturate the probe cap"
        );
    }
    eprintln!(
        "tie-term deep counts: 乌篷巷 {} / 百步桥 {} / 栖霞圩 {} (chapter order 72/84/96)",
        deep("乌篷巷"),
        deep("百步桥"),
        deep("栖霞圩")
    );
    assert!(
        deep("乌篷巷") < deep("百步桥") && deep("百步桥") < deep("栖霞圩"),
        "the tie terms keep the designed chunk-df order (the stage-two argmin raw material)"
    );
    assert!(
        deep(PROTAGONIST) >= CHAPTERS as u64,
        "protagonist rides every chapter"
    );

    // ---- the query face: scene reachable ungated (A-R8 recall half),
    // the FM2 flooding OBSERVED (printed, never asserted — R8's stance is
    // bypass, not therapy) ----
    let e1 = &EVENTS[0];
    let v = search_json(&x, e1.query, 50);
    let hits = v["hits"].as_array().expect("hits");
    let sections: Vec<u64> = hits
        .iter()
        .filter(|h| h["location"]["key"].as_str() == Some(NOVEL_KEY))
        .filter_map(|h| h["location"]["coord"]["section"].as_u64())
        .collect();
    assert!(
        sections.contains(&(e1.occurrence as u64)),
        "the occurrence chapter {} must be reachable in the ungated top-50 (got sections {:?})",
        e1.occurrence,
        sections
    );
    let recap_secs: Vec<u64> = e1.recaps.iter().map(|(s, _)| *s as u64).collect();
    let above = sections
        .iter()
        .filter(|s| recap_secs.contains(s))
        .filter(|s| **s != e1.occurrence as u64)
        .count();
    eprintln!(
        "FM2 observation: event {} occurrence section {} at ungated rank, {} recap-section hits \
         above/beside it in the top-50 — the recap-flooded ranking the corpus encodes",
        e1.label, e1.occurrence, above
    );
    // T17: the CJK query's anchors member is NON-empty now, head = the
    // quoted keyword (the ar7 pin asserts it in full)
    eprintln!("anchors under T17: {:?}", v["anchors"]);
    assert_eq!(
        v["anchors"][0],
        serde_json::json!(e1.keyword),
        "the T17 head anchor: the quoted event keyword (the argmin)"
    );
}

// ---------------------------------------------------------------------------
// the env-gated behavior pins (T17's analyze()-based pipeline landed;
// T18's orderBy pin still awaits its activation — the #[ignore] reasons
// name the env conditions)
// ---------------------------------------------------------------------------

/// A-R7 (design §1 D-11.2): CJK event queries get anchors and the HEAD
/// anchor is the expected entity — the T17 payoff, live at the indexed
/// corpus (`LexicalKernel::analyze()` + the D-8.3 candidate rewrite +
/// the D-8.4 two-stage argmin head anchor). The fixture makes the head
/// decidable WITHOUT segmentation luck: the keyword is quoted (priority
/// candidate, probed whole), its characters are quarantined (probe = the
/// event's own chunk count), and every other query word has zero corpus
/// occurrences (df=0 never gates) — the quoted keyword is trivially the
/// argmin. (v1 history: these queries analyzed to `anchors: []` — the
/// record this test replaced.)
#[test]
#[ignore = "needs the indexed narrative corpus; set VF_NARRATIVE_MODELS_DIR (see module doc)"]
fn narrative_ar7_cjk_event_head_anchors() {
    let x = stand_indexed_narrative("ar7");
    for e in EVENTS.iter().filter(|e| e.label != "MeridianVow") {
        let v = search_json(&x, e.query, 20);
        let anchors = v["anchors"].as_array().cloned().unwrap_or_default();
        assert!(
            !anchors.is_empty(),
            "{}: CJK queries must analyze to anchors under T17 (got {:?})",
            e.label,
            v["anchors"]
        );
        assert_eq!(
            anchors[0],
            serde_json::json!(e.keyword),
            "{}: the head anchor is the quoted event keyword (the argmin-df candidate)",
            e.label
        );
        assert!(
            has_hits(&v),
            "{}: the gated search must still answer",
            e.label
        );
    }
}

/// The F1 regression guard at the indexed corpus: the Latin identifier
/// event's head anchor is `MeridianVow`'s analyzer token — under T17 the
/// candidate carries the index's own spelling (`meridianvow`, lowercased
/// by the shared pipeline; the raw probe and the token hit the same
/// postings), and the verdict is exactly that one term (the query's CJK
/// words are zero-words, df=0). The guard property is unchanged through
/// the rewrite: the one shape that already worked still gates on exactly
/// its identifier.
#[test]
#[ignore = "needs the indexed narrative corpus; set VF_NARRATIVE_MODELS_DIR (F1 guard, design D-11.1)"]
fn narrative_ar7_latin_identifier_guard() {
    let x = stand_indexed_narrative("f1");
    let e = &EVENTS[1]; // MeridianVow
    let v = search_json(&x, e.query, 20);
    assert_eq!(
        v["anchors"],
        serde_json::json!(["meridianvow"]),
        "the Latin identifier event gates on exactly its identifier (the \
         analyzer's token spelling)"
    );
    assert!(has_hits(&v));
}

/// The structure-word flood pin (T17 恒门): the retired "all-structure ⇒
/// zero anchors ⇒ ungated" escape hatch stays retired — a pure furniture
/// query MUST gate (anchors non-empty, head = a furniture bigram, gated
/// result non-empty). The furniture bigrams all live in the refrain's
/// own chunks (an exact tie ⇒ first-seen wins), so membership, not
/// which one, is the assertion.
#[test]
#[ignore = "needs the indexed narrative corpus; set VF_NARRATIVE_MODELS_DIR (see module doc)"]
fn narrative_ar7_structure_flood_constant_gate() {
    let x = stand_indexed_narrative("flood");
    let v = search_json(&x, FLOOD_QUERY, 50);
    let anchors = v["anchors"].as_array().cloned().unwrap_or_default();
    assert!(
        !anchors.is_empty(),
        "the all-structure query must gate under T17 (no zero-anchor escape): {:?}",
        v["anchors"]
    );
    // every furniture bigram rides the same refrain chunks (an exact tie
    // at saturation ⇒ first-seen wins); assert membership, not which one
    assert!(
        anchors[0].is_string()
            && FURNITURE_BIGRAMS
                .iter()
                .any(|bg| anchors[0].as_str().unwrap().contains(bg)),
        "the flood head anchor is a furniture bigram: {:?}",
        anchors[0]
    );
    assert!(has_hits(&v));
}

/// The two-stage saturation tie (T17, D-8.4): all three quoted tie terms
/// cover ≥ 64 chapters, so every stage-one probe reads the cap (a three-way
/// tie AT the head) and only the stage-two exact counts can pick argmin =
/// 乌篷巷 (72 chapters < 84 < 96). The dictionary subwords (百步/乌篷)
/// ride the same saturation and the same exact counts, tying their
/// parents — first-seen keeps the quoted whole-span spellings ahead.
#[test]
#[ignore = "needs the indexed narrative corpus; set VF_NARRATIVE_MODELS_DIR (see module doc)"]
fn narrative_probe_saturation_tie_two_stage() {
    let x = stand_indexed_narrative("tie");
    // the raw material first (true at HEAD too): saturation + deep order
    let storage = Storage::file(&x.data_dir);
    let ix = first_index(&x.data_dir);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio rt");
    let kernel = vectorfs_server::lexical::kernel();
    for (term, _, _) in TIE_TERMS.iter() {
        let capped = rt
            .block_on(kernel.probe_df(&storage, "docs", &ix, term))
            .unwrap();
        assert_eq!(
            capped,
            vectorfs_server::lexical::PROBE_DF_LIMIT as u64,
            "{term}: stage-one probe saturates (the tie's precondition)"
        );
    }
    let v = search_json(&x, TIE_QUERY, 20);
    let anchors = v["anchors"].as_array().cloned().unwrap_or_default();
    assert!(
        !anchors.is_empty(),
        "the tie query must analyze to anchors under T17: {:?}",
        v["anchors"]
    );
    assert_eq!(
        anchors[0],
        serde_json::json!("乌篷巷"),
        "stage-two exact counts must pick the 72-chapter term as argmin head"
    );
}

/// The MAX_PROBES truncation pin (T17, D-8.4 规范语义): the long query's
/// multi-char analyzer terms (连夜/跑遍/南浦/津渡/码头/回生/药房/船夫…)
/// fill the probe set to exactly MAX_PROBES, and the one-shot name 谢闻笛
/// — the query's rarest possible anchor (df = one chapter) — never
/// enters it: the icu dictionary segments the coined name into single
/// characters (dropped by the candidate filter), and even its would-be
/// position sits past the truncation. A discriminative entity the probe
/// set never reaches is 既定行为, not a bug — the query still gates on
/// its first-eight argmin.
#[test]
#[ignore = "needs the indexed narrative corpus; set VF_NARRATIVE_MODELS_DIR (see module doc)"]
fn narrative_long_query_probe_truncation() {
    let x = stand_indexed_narrative("long");
    let v = search_json(&x, LONG_QUERY, 50);
    let anchors = v["anchors"].as_array().cloned().unwrap_or_default();
    assert!(
        !anchors.is_empty(),
        "the long query still gates on its first-eight candidates: {:?}",
        v["anchors"]
    );
    assert!(
        anchors
            .iter()
            .all(|a| !a.as_str().is_some_and(|s| s.contains("谢闻笛"))),
        "the 9th+-position one-shot name is beyond MAX_PROBES and must never anchor: {:?}",
        anchors
    );
}

/// A-R8 position half (T18, D-11.2): with the section axis applied to the
/// event query's hit set, the FIRST event-chapter hit in (section,
/// line-start) order is the occurrence chapter — no recap may claim the
/// earliest narrative slot despite its higher BM25 (复述不冒名).
///
/// ACTIVATION (T18): `/x-vf Search` `orderBy: "section-asc"` (D-10.4)
/// replaces the client-side sort below — assert the RESPONSE order
/// directly then. Today this pins the metadata ingredients (every novel
/// hit carries coord.section) plus the ordering semantics pre-implemented
/// client-side; it is green on an indexed corpus by construction, and the
/// #[ignore] belongs to T18's activation set.
#[test]
#[ignore = "activates with T18 (narrative-recall design §2): orderBy section-asc (D-10.4)"]
fn narrative_ar8_position_half_section_order() {
    let x = stand_indexed_narrative("ar8");
    let e = &EVENTS[0]; // 潮汐灯, occurrence 7
    let v = search_json(&x, e.query, 50);
    let mut event_hits: Vec<(u64, u64)> = v["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .filter(|h| h["location"]["key"].as_str() == Some(NOVEL_KEY))
        .filter_map(|h| {
            let section = h["location"]["coord"]["section"].as_u64()?;
            let line = h["location"]["coord"]["line"]["start"]
                .as_u64()
                .unwrap_or(0);
            Some((section, line))
        })
        .filter(|(s, _)| *s == e.occurrence as u64 || e.recaps.iter().any(|(r, _)| *r as u64 == *s))
        .collect();
    assert!(
        !event_hits.is_empty(),
        "the event query must surface its chapters"
    );
    event_hits.sort_unstable();
    assert_eq!(
        event_hits[0].0, e.occurrence as u64,
        "the occurrence chapter leads the section-ordered view — recaps do not claim the slot"
    );
    // TODO(T18): switch to server-side orderBy:"section-asc" and assert
    // the response's first event hit IS the occurrence chunk verbatim.
}

/// The FM4 cross-source observation (design §1 D-11.2, 降级为观测): the
/// structure-dense decoy competes into furniture-query results from
/// outside the novel. Observed and REPORTED — the mechanism adjudication
/// is explicitly deferred (design §5.5), so this records the noise floor
/// the future ruling will argue from.
#[test]
#[ignore = "needs the indexed narrative corpus; set VF_NARRATIVE_MODELS_DIR (FM4 observation, design D-11.2)"]
fn narrative_fm4_cross_source_decoy_observation() {
    let x = stand_indexed_narrative("fm4");
    let v = search_json(&x, FLOOD_QUERY, 50);
    let hits = v["hits"].as_array().expect("hits");
    let decoy_hits = hits
        .iter()
        .filter(|h| h["location"]["key"].as_str() == Some(DECOY_KEY))
        .count();
    eprintln!(
        "FM4 observation: {decoy_hits}/{} top-50 flood-query hits come from the decoy source \
         (cross-source noise floor; mechanism adjudication deferred — design §5.5)",
        hits.len()
    );
    assert!(
        decoy_hits > 0,
        "the structure-dense decoy must compete into furniture results (the observed FM4 shape)"
    );
}
