//! Calibrates [`SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS`], and re-reads the query-side floor in
//! the same space.
//!
//! Every other measurement in this suite asks a query-shaped question: a probe goes in, a ranked
//! list comes out, and the floor decides how much of the list is worth showing. The second hop asks
//! a different one. It starts from a Context the session already touched and asks which *other*
//! Contexts belong beside it -- document against document, no query in the picture -- and the answer
//! is admitted or refused rather than ranked. So the number it needs is not the query floor
//! measured again; it is a different statistic of a different distribution, and this file measures
//! it.
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
//! The second test reuses the same corpus for the query side. The fixture's `intents` are Working
//! Intents in the shape a real session produces -- goal, current direction and in-scope list -- and
//! joining them with a space is what `semantic_query_text` does before handing the string to the
//! provider. Scoring those against the *cross-topic* Contexts says how much of this corpus
//! [`QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS`] admits when the Contexts are about something
//! else, which is the number that decides whether that floor is still load-bearing.
//!
//! That second measurement has a limit worth stating where it is taken rather than where it is
//! quoted: these Intents were written alongside these Contexts, so they share vocabulary that a
//! real Working Intent -- written before the work is understood, describing a process rather than
//! the knowledge -- does not. The number it produces is therefore an *optimistic* bound on the
//! intent path, and the pessimistic one comes from the device run recorded in ADR-0007. Both point
//! the same way, which is the only claim made from either.

#![cfg(all(feature = "embedding-onnx", unix))]

mod f2llm_snapshot;

use std::sync::OnceLock;

use f2llm_snapshot::provider;
use sctx_search::{
    EmbeddingProvider, QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS,
    SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS, embedding::cosine_similarity,
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
/// reasoning that gave up two positives when [`QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS`] was
/// set, and it applies here unchanged. This is the margin, not the raw gap to the next positive
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

#[test]
#[ignore = "needs a real F2LLM-v2-0.6B snapshot; see the module docs"]
fn the_query_floor_admits_most_of_a_cross_topic_corpus() {
    let corpus = corpus();
    let provider = provider().as_ref();

    println!("\n--- Working Intent against the whole corpus (basis points) ---");
    println!(
        "{:<26} {:>9} {:>9} {:>9} {:>13} {:>13}",
        "intent", "best(on)", "best(off)", "gap", "off>=2800", "off>=hop2"
    );

    let mut total_off_topic = 0_usize;
    let mut admitted_off_topic = 0_usize;
    let mut admitted_off_topic_at_hop2 = 0_usize;
    let mut worst_inversion: Option<(String, u16, u16)> = None;

    for intent in fixture()["intents"]
        .as_array()
        .expect("the fixture holds an intent array")
    {
        // What `semantic_query_text` builds: goal, current direction, in-scope list, joined with a
        // space. Out-of-scope is deliberately absent there and therefore absent here.
        let mut parts = vec![
            intent["goal"].as_str().expect("an intent carries a goal"),
            intent["current_direction"]
                .as_str()
                .expect("an intent carries a direction"),
        ];
        parts.extend(
            intent["in_scope"]
                .as_array()
                .expect("an intent carries an in-scope list")
                .iter()
                .map(|scope| scope.as_str().expect("in-scope holds strings")),
        );
        let vector = provider
            .encode(&parts.join(" "))
            .expect("every intent encodes");
        let topic = intent["topic"].as_str().expect("an intent carries a topic");

        let mut best_on = 0_u16;
        let mut best_off = 0_u16;
        let mut off_topic = 0_usize;
        let mut off_admitted = 0_usize;
        let mut off_admitted_hop2 = 0_usize;
        for context in corpus {
            let score = basis_points(cosine_similarity(&vector, &context.vector));
            if context.topic == topic {
                best_on = best_on.max(score);
            } else {
                best_off = best_off.max(score);
                off_topic += 1;
                if score >= QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS {
                    off_admitted += 1;
                }
                if score >= SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS {
                    off_admitted_hop2 += 1;
                }
            }
        }

        let id = intent["id"].as_str().expect("an intent carries an id");
        println!(
            "{:<26} {best_on:>9} {best_off:>9} {:>9} {:>13} {:>13}",
            id,
            i32::from(best_on) - i32::from(best_off),
            format!("{off_admitted}/{off_topic}"),
            format!("{off_admitted_hop2}/{off_topic}")
        );

        total_off_topic += off_topic;
        admitted_off_topic += off_admitted;
        admitted_off_topic_at_hop2 += off_admitted_hop2;
        if best_off >= best_on {
            worst_inversion = Some((id.to_owned(), best_on, best_off));
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let rate = admitted_off_topic as f64 / total_off_topic as f64 * 100.0;
    #[allow(clippy::cast_precision_loss)]
    let hop2_rate = admitted_off_topic_at_hop2 as f64 / total_off_topic as f64 * 100.0;
    println!(
        "\nfloor {QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS} admits \
         {admitted_off_topic}/{total_off_topic} ({rate:.1}%) of the off-topic corpus; the \
         document-side floor {SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS} would admit \
         {admitted_off_topic_at_hop2}/{total_off_topic} ({hop2_rate:.1}%)"
    );
    if let Some((id, on, off)) = &worst_inversion {
        println!("an off-topic Context outscores every on-topic one for {id}: {off} against {on}");
    }

    // Not an assertion about the floor's value, which this corpus cannot settle: the fixture holds
    // no noise queries, and the query floor's remaining job -- explicit `context_search` -- is a
    // path where a caller asked for a broad list. What is asserted is that the number this test
    // exists to publish was computed over the denominator the fixture describes, because a rate
    // quoted in a doc comment is worth exactly what its denominator is.
    let expected_off_topic = fixture()["intents"]
        .as_array()
        .expect("the fixture holds an intent array")
        .iter()
        .map(|intent| {
            corpus
                .iter()
                .filter(|context| context.topic != intent["topic"].as_str().unwrap_or_default())
                .count()
        })
        .sum::<usize>();
    assert_eq!(
        total_off_topic, expected_off_topic,
        "the off-topic denominator is not the corpus this fixture describes"
    );
}
