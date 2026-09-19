//! Calibrates [`SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS`].
//!
//! Every earlier measurement in this suite asked a query-shaped question: a probe went in, a
//! ranked list came out, and a floor decided how much of the list was worth showing. The second
//! hop asks a different one. It starts from a Context the session already touched and asks which
//! *other* Contexts belong beside it -- document against document, no query in the picture -- and
//! the answer is admitted or refused rather than ranked. So the number it needs is not a query
//! floor measured again; it is a different statistic of a different distribution, and this file
//! measures it.
//!
//! This file used to hold a second test that re-read the query-side floor over the same corpus.
//! Its finding is what retired that floor: ranking was intact on every Intent -- the best
//! on-topic Context beat the best off-topic one by 2902 to 5584 basis points, without exception --
//! while the floor itself admitted 31 of 96 off-topic Contexts, 32.3%. A ranking signal was being
//! asked to make an admission decision, and no value of that constant could have made it one.
//! ADR-0007 replaced the path, its amendment records the retirement, and the test went with the
//! constant it measured.
//!
//! `#[ignore]`d because it needs roughly 2.4 GB of weights this repository deliberately does not
//! ship. Point it at a Hugging Face snapshot and the ONNX Runtime library:
//!
//! ```text
//! SCTX_PROBE_F2LLM_MODEL=~/.cache/huggingface/hub/models--codefuse-ai--F2LLM-v2-0.6B/snapshots/<sha> \
//! SCTX_PROBE_EMBEDDING_RUNTIME=~/.shared-context/embedding/runtime/libonnxruntime.dylib \
//!   cargo test --release -p sctx-search --test embedding_hop2_admission_calibration \
//!     -- --ignored --nocapture
//! ```
//!
//! ## What is measured
//!
//! The corpus is `fixtures/association/hard-negative-v1.json`, whose whole construction is the
//! measurement: three topic families across two repository labels, so that every pair of Contexts
//! falls into one of four groups by whether the two share a repository and whether they share a
//! topic. Same-topic pairs are what a second hop exists to reach; cross-topic pairs are what it has
//! to refuse. The binding cases are the two diagonals -- cross-repo/same-topic, which is the value
//! the hop delivers and also its weakest positive, and same-repo/cross-topic, which is its hardest
//! negative because two Contexts in one repository share the product's whole vocabulary while
//! describing unrelated work.
//!
//! Both sides are encoded through the *document* path, as `statement` and `rationale` joined with a
//! newline -- the text [`sctx_search::SearchEngine::embeddable_revisions`] builds on the fields
//! these fixtures fill. That symmetry is the point: a document-to-document comparison is what the
//! second hop performs, and scoring one side through the query path would measure a number the hop
//! never computes.
//!

#![cfg(all(feature = "embedding-onnx", unix))]

mod f2llm_snapshot;

use std::sync::OnceLock;

use f2llm_snapshot::provider;
use sctx_search::{
    EmbeddingProvider, SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS, embedding::cosine_similarity,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("../../../fixtures/association/hard-negative-v1.json");

/// Contexts the fixture holds, so a change in the corpus is not read as a change in separation.
const TOTAL_CONTEXTS: usize = 24;

/// Ranking-quality ratchet over the whole pair set: same-topic pairs against cross-topic pairs.
///
/// A ratchet, not a target. It is the value measured by the run recorded in the constant's doc
/// comment, rounded down to the nearest ten basis points. Recalibrating deliberately means moving
/// this line in the same commit as the constant and saying why.
///
/// It is reported and ratcheted but never used to derive the floor, because an AUC says the two
/// distributions are *ordered* and says nothing about whether one cut separates them. Confusing
/// those two readings is the diagnosis this whole redesign rests on: the channel has always ranked
/// well and admitted badly.
const MEASURED_AUC_BASIS_POINTS: u16 = 9_090;

/// How far above the highest cross-topic pair the floor must sit.
///
/// A threshold defended by a handful of basis points is an overfit to one fixture row -- the
/// reasoning that gave up two positives when the retired query-side floor for this family (2800)
/// was set, and it applies here unchanged. This is the margin, not the raw gap to the next positive
/// above the ceiling: in a dense distribution that gap is a few basis points wide by construction
/// (9 here, 60 on the device corpus) and says nothing about robustness.
const MINIMUM_CEILING_MARGIN_BASIS_POINTS: u16 = 100;

/// Cross-repo/same-topic pairs the floor admits, out of the pairs of that group the fixture holds.
///
/// The second hop's reason to exist is the cross-repository join -- the Android Context that
/// explains the field the web Context consumes -- so this is the recall number that matters, and a
/// floor that quietly stops delivering it should fail here rather than pass quietly.
///
/// It is deliberately low as a fraction. This fixture's topic families span a topic's whole
/// history, where a real second hop starts from a Context the session just touched and reaches the
/// few Contexts written around it; the device corpus, whose same-topic family was one feature's
/// work, retains 95% at a comparable floor. Both numbers are recorded in the constant's doc
/// comment, and the distance between them is a property of the two corpora rather than a
/// disagreement about the floor.
const RETAINED_CROSS_REPO_SAME_TOPIC: usize = 13;

/// One Context, encoded.
struct Encoded {
    label: String,
    repo: String,
    topic: String,
    vector: Vec<f32>,
}

/// One pair group, named by the two axes that define it.
// The shared `Topic` postfix is one of the two axes that define a group, not filler: dropping it
// would leave `SameRepo` and `CrossRepo` naming four things that differ on the axis the name no
// longer mentions.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Group {
    SameRepoSameTopic,
    CrossRepoSameTopic,
    SameRepoCrossTopic,
    CrossRepoCrossTopic,
}

impl Group {
    const ALL: [Self; 4] = [
        Self::SameRepoSameTopic,
        Self::CrossRepoSameTopic,
        Self::SameRepoCrossTopic,
        Self::CrossRepoCrossTopic,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::SameRepoSameTopic => "same-repo/same-topic",
            Self::CrossRepoSameTopic => "cross-repo/same-topic",
            Self::SameRepoCrossTopic => "same-repo/cross-topic",
            Self::CrossRepoCrossTopic => "cross-repo/cross-topic",
        }
    }

    /// Whether the second hop is supposed to admit this group.
    const fn same_topic(self) -> bool {
        matches!(self, Self::SameRepoSameTopic | Self::CrossRepoSameTopic)
    }

    fn of(left: &Encoded, right: &Encoded) -> Self {
        match (left.repo == right.repo, left.topic == right.topic) {
            (true, true) => Self::SameRepoSameTopic,
            (false, true) => Self::CrossRepoSameTopic,
            (true, false) => Self::SameRepoCrossTopic,
            (false, false) => Self::CrossRepoCrossTopic,
        }
    }
}

/// One scored Context pair.
struct Pair {
    group: Group,
    score: u16,
    left: String,
    right: String,
}

/// Cosine in basis points, by the same rounding the channel applies before comparing to a floor.
fn basis_points(similarity: f32) -> u16 {
    if !similarity.is_finite() || similarity <= 0.0 {
        return 0;
    }
    let scaled = (similarity * 10_000.0).round();
    if scaled >= 10_000.0 {
        return 10_000;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        scaled as u16
    }
}

/// The text a fixture Context contributes to the embedding index.
///
/// `statement` and `rationale` joined with a newline, which is `embeddable_revisions`' output on a
/// revision with no `problem_view` -- the shape every Context in this fixture has.
fn corpus_text(context: &Value) -> String {
    ["statement", "rationale"]
        .into_iter()
        .filter_map(|field| context.get(field).and_then(Value::as_str))
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The fixture parsed once, so both tests read the same JSON.
fn fixture() -> &'static Value {
    static FIXTURE_JSON: OnceLock<Value> = OnceLock::new();
    FIXTURE_JSON.get_or_init(|| {
        serde_json::from_str(FIXTURE).expect("the hard-negative fixture is valid JSON")
    })
}

/// The whole corpus encoded through the document path, once per test binary.
///
/// Both tests in this file score against every Context, and the weights are loaded once; encoding
/// the corpus twice would add minutes and measure the same vectors.
fn corpus() -> &'static Vec<Encoded> {
    static CORPUS: OnceLock<Vec<Encoded>> = OnceLock::new();
    CORPUS.get_or_init(|| {
        let provider = provider().as_ref();
        fixture()["contexts"]
            .as_array()
            .expect("the fixture holds a context array")
            .iter()
            .map(|context| Encoded {
                label: context["label"]
                    .as_str()
                    .expect("a context carries a label")
                    .to_owned(),
                repo: context["repo"]
                    .as_str()
                    .expect("a context carries a repo")
                    .to_owned(),
                topic: context["topic"]
                    .as_str()
                    .expect("a context carries a topic")
                    .to_owned(),
                vector: provider
                    .encode_bulk(&corpus_text(context))
                    .expect("every corpus text encodes"),
            })
            .collect()
    })
}

/// Every unordered pair of Contexts, scored and grouped.
fn pairs() -> Vec<Pair> {
    let corpus = corpus();
    let mut pairs = Vec::with_capacity(corpus.len() * (corpus.len() - 1) / 2);
    for (index, left) in corpus.iter().enumerate() {
        for right in &corpus[index + 1..] {
            pairs.push(Pair {
                group: Group::of(left, right),
                score: basis_points(cosine_similarity(&left.vector, &right.vector)),
                left: left.label.clone(),
                right: right.label.clone(),
            });
        }
    }
    pairs
}

/// Nearest-rank percentile over an already sorted slice.
fn percentile(sorted: &[u16], fraction: f64) -> u16 {
    if sorted.is_empty() {
        return 0;
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rank = ((fraction * sorted.len() as f64).ceil() as usize).max(1) - 1;
    sorted[rank.min(sorted.len() - 1)]
}

/// Mann-Whitney AUC in basis points: same-topic pairs ranked against cross-topic pairs.
///
/// Reported rather than asserted on its own, because an AUC says the two distributions are ordered
/// and says nothing about whether a single cut separates them. The clean band below is the
/// statistic a threshold actually rests on; this one says whether looking for a threshold is
/// reasonable at all.
fn auc_basis_points(pairs: &[Pair]) -> u16 {
    let positives = pairs
        .iter()
        .filter(|pair| pair.group.same_topic())
        .map(|pair| pair.score)
        .collect::<Vec<_>>();
    let negatives = pairs
        .iter()
        .filter(|pair| !pair.group.same_topic())
        .map(|pair| pair.score)
        .collect::<Vec<_>>();
    let mut wins = 0.0_f64;
    for positive in &positives {
        for negative in &negatives {
            if positive > negative {
                wins += 1.0;
            } else if positive == negative {
                wins += 0.5;
            }
        }
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    {
        ((wins / (positives.len() * negatives.len()) as f64) * 10_000.0).round() as u16
    }
}

fn print_distribution(pairs: &[Pair]) {
    println!("\n--- pair similarity by group (basis points) ---");
    println!(
        "{:<22} {:>4} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6}",
        "group", "n", "min", "p25", "p50", "p75", "p90", "max", "mean"
    );
    for group in Group::ALL {
        let mut scores = pairs
            .iter()
            .filter(|pair| pair.group == group)
            .map(|pair| pair.score)
            .collect::<Vec<_>>();
        scores.sort_unstable();
        let total = scores.iter().map(|score| u32::from(*score)).sum::<u32>();
        let mean =
            f64::from(total) / f64::from(u32::try_from(scores.len()).expect("groups are small"));
        println!(
            "{:<22} {:>4} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6.0}",
            group.name(),
            scores.len(),
            scores.first().copied().unwrap_or_default(),
            percentile(&scores, 0.25),
            percentile(&scores, 0.50),
            percentile(&scores, 0.75),
            percentile(&scores, 0.90),
            scores.last().copied().unwrap_or_default(),
            mean
        );
    }
}

/// Prints what each candidate floor would admit, so the chosen value reads as a choice.
fn print_sweep(pairs: &[Pair], centre: u16) {
    println!("\n--- candidate floors ---");
    println!(
        "{:>6}  {:>21}  {:>21}  {:>17}",
        "floor", "recall(xr/same-topic)", "recall(sr/same-topic)", "FP(cross-topic)"
    );
    let first = centre.saturating_sub(500);
    for step in 0..11_u16 {
        let floor = first + step * 100;
        let admitted = |group: Group| {
            let total = pairs.iter().filter(|pair| pair.group == group).count();
            let kept = pairs
                .iter()
                .filter(|pair| pair.group == group && pair.score >= floor)
                .count();
            format!("{kept}/{total}")
        };
        let negatives = pairs.iter().filter(|pair| !pair.group.same_topic()).count();
        let false_positives = pairs
            .iter()
            .filter(|pair| !pair.group.same_topic() && pair.score >= floor)
            .count();
        println!(
            "{floor:>6}  {:>21}  {:>21}  {:>17}",
            admitted(Group::CrossRepoSameTopic),
            admitted(Group::SameRepoSameTopic),
            format!("{false_positives}/{negatives}")
        );
    }
}

#[test]
#[ignore = "needs a real F2LLM-v2-0.6B snapshot; see the module docs"]
fn the_second_hop_floor_sits_in_a_band_no_cross_topic_pair_reaches() {
    let pairs = pairs();
    assert_eq!(
        corpus().len(),
        TOTAL_CONTEXTS,
        "the fixture changed shape; the ratchets below are no longer comparable"
    );

    print_distribution(&pairs);

    // The clean band: above every cross-topic pair, below the next same-topic pair. Its floor is
    // the lowest threshold with no false positive, its ceiling is the highest threshold that gives
    // up nothing more than that floor already does.
    let cross_topic_ceiling = pairs
        .iter()
        .filter(|pair| !pair.group.same_topic())
        .map(|pair| pair.score)
        .max()
        .expect("the fixture holds cross-topic pairs");
    let band_ceiling = pairs
        .iter()
        .filter(|pair| pair.group.same_topic() && pair.score > cross_topic_ceiling)
        .map(|pair| pair.score)
        .min()
        .expect("some same-topic pair outscores every cross-topic pair");
    let band = band_ceiling - cross_topic_ceiling;
    let auc = auc_basis_points(&pairs);

    println!(
        "\ncross-topic ceiling {cross_topic_ceiling}, next same-topic pair {band_ceiling}, clean \
         band {band} bp, AUC {auc} bp, floor in the source \
         {SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS}"
    );

    print_sweep(&pairs, SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS);

    println!("\n--- the cross-topic pairs that come closest (false-positive risk) ---");
    let mut hardest = pairs
        .iter()
        .filter(|pair| !pair.group.same_topic())
        .collect::<Vec<_>>();
    hardest.sort_by_key(|pair| std::cmp::Reverse(pair.score));
    for pair in hardest.iter().take(8) {
        println!(
            "{:>6}  [{}] {} x {}",
            pair.score,
            pair.group.name(),
            pair.left,
            pair.right
        );
    }

    println!("\n--- the same-topic pairs the floor gives up (false negatives) ---");
    let mut given_up = pairs
        .iter()
        .filter(|pair| {
            pair.group.same_topic() && pair.score < SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS
        })
        .collect::<Vec<_>>();
    given_up.sort_by_key(|pair| std::cmp::Reverse(pair.score));
    for pair in &given_up {
        println!(
            "{:>6}  [{}] {} x {}",
            pair.score,
            pair.group.name(),
            pair.left,
            pair.right
        );
    }

    assert!(
        auc >= MEASURED_AUC_BASIS_POINTS,
        "same-topic and cross-topic pairs separate at AUC {auc}, below the \
         {MEASURED_AUC_BASIS_POINTS} this calibration measured: the corpus join, the encoder or the \
         fixture changed"
    );

    // The hard constraint, and the reason this hop is an admission rather than a rank. A Context
    // admitted here is injected without any further test of relevance, so one cross-topic pair
    // above the floor is one off-topic Context in front of an Agent who is working on something
    // else -- the outcome this repository has consistently held to be worse than retrieving
    // nothing.
    let false_positives = pairs
        .iter()
        .filter(|pair| {
            !pair.group.same_topic() && pair.score >= SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS
        })
        .count();
    assert_eq!(
        false_positives, 0,
        "the floor {SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS} admits {false_positives} \
         cross-topic pairs; the cross-topic ceiling is {cross_topic_ceiling}"
    );
    assert!(
        SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS
            >= cross_topic_ceiling + MINIMUM_CEILING_MARGIN_BASIS_POINTS,
        "the floor clears the cross-topic ceiling {cross_topic_ceiling} by less than \
         {MINIMUM_CEILING_MARGIN_BASIS_POINTS} bp, which makes it a reading of the single hardest \
         pair rather than a threshold"
    );

    let cross_repo_total = pairs
        .iter()
        .filter(|pair| pair.group == Group::CrossRepoSameTopic)
        .count();
    let cross_repo_kept = pairs
        .iter()
        .filter(|pair| {
            pair.group == Group::CrossRepoSameTopic
                && pair.score >= SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS
        })
        .count();
    println!("cross-repo/same-topic pairs admitted: {cross_repo_kept}/{cross_repo_total}");
    assert!(
        cross_repo_kept >= RETAINED_CROSS_REPO_SAME_TOPIC,
        "the floor admits {cross_repo_kept}/{cross_repo_total} cross-repository joins, below the \
         {RETAINED_CROSS_REPO_SAME_TOPIC} this calibration measured"
    );
}
