# Post-Classical Heuristic Source-Code Metrics for mehen

**Project:** mehen source code metrics analytics
**Target modules:** shared `mehen-metrics` + per-language crates; new history layer in/around `mehen-git`
**Primary use case:** CI/diff analytics, repository health reporting, and top-offender identification
**Document status:** research foundation and metric design proposal (candidate additions)
**Last updated:** 2026-07-10

---

## 1. Executive summary

mehen already implements a strong classical static suite: cyclomatic, cognitive (SonarSource
model), the LOC family (SLOC/PLOC/LLOC/CLOC/blank), full Halstead, three Maintainability Index
variants, ABC, NARGS, NOM, NEXIT, NPA, NPM, and WMC, plus rich `sql.*` and `markdown.*`
namespaces. What it does **not** yet have falls into four research areas surveyed here. This
document proposes concrete additions and, for each candidate, a *fit card* stating what it
measures, its primary citation, whether it is single-file and deterministic, its dependency /
trained-artifact requirements, and a rough implementation effort (S/M/L/XL).

The headline recommendations, in priority order:

1. **Git/history process metrics are the single biggest gap and the best fit.** mehen's
   `mehen-git` crate today only diffs *which* files changed; it computes no history metrics at
   all. Churn, code age, author count / ownership, and change (temporal) coupling are
   deterministic, language-agnostic, cheap, and — per multiple large empirical studies — better
   defect predictors than any static code metric. They also match mehen's existing diff and
   top-offender reporting perfectly. **This is the highest-value area and warrants a dedicated
   design pass.** (§6)

2. **Two deterministic, no-model structural metrics are strong, non-redundant additions:**
   Shannon **textual entropy** and **structural (AST-edge) entropy** (Torres et al., EMSE 2025),
   which correlate only weakly with cyclomatic complexity, and **DepDegree** (Beyer & Fararooy,
   ICPC 2010), a use-def-graph edge count that discriminates code that cyclomatic complexity and
   statement count treat as identical. (§3)

3. **Among learned readability models, only Posnett et al. (2011) is directly portable** — it
   ships published logistic-regression coefficients and needs no trained artifact at inference.
   Buse & Weimer (2010) and Scalabrino et al. (2016/2018) require shipping trained classifier
   weights and, for Scalabrino, NLP machinery. (§4)

4. **Code "naturalness" (Hindle cross-entropy) is powerful but a poor architectural fit** — it is
   explicitly *not* single-file: it needs a corpus-trained, per-language n-gram model plus a lexer,
   which clashes with mehen's dependency-light / deterministic-across-platforms contract unless a
   frozen model artifact is bundled per language. (§5)

**One cross-cutting caveat governs everything below.** A rigorous IEEE TSE 2019 study
(Scalabrino et al.) tested 121 code/documentation/developer metrics against 444 human
understandability judgments and found that *none* correlated significantly with perceived or
actual understandability — not even readability and complexity metrics — and that modest ML
combinations remained too inaccurate for practical use. mehen must therefore present any of these
as **review-prioritization / risk signals**, never as an "understandability" or "quality" score.
(§7)

---

## 2. How to read this document

### 2.1 Fit-card fields

Every candidate metric carries a fit card:

| Field | Meaning |
|---|---|
| **Measures** | The quantity computed, in one line. |
| **Primary source** | The citation the definition is drawn from. |
| **Single-file?** | Whether it can be computed from one file in isolation (mehen's default unit) or needs cross-file / repository context. |
| **Deterministic?** | Whether the same input always yields the same output on every platform — mehen's hard contract. |
| **External deps / trained artifact** | What must be shipped or linked beyond an AST/CST walk (a trained model, coefficients, a git repo, a lexer, etc.). |
| **Effort** | S (≈ a visitor pass or counter), M (new accumulator + intra-procedural analysis), L (new subsystem), XL (new subsystem + shipped model artifact). |
| **Verification** | How strongly the research pipeline confirmed the *definitional* claim: **3-0** = unanimous adversarial pass; **primary-sourced** = drawn from a primary source but not put through the 3-vote gate (see §8). |

### 2.2 mehen's fit constraints (from the current architecture)

The catalog of the current codebase established the "shape" any new metric must fit:

- **Open key namespace.** `MetricKey(SmolStr)` (`crates/mehen-core/src/metric_key.rs`) means any
  `family.subkey` string is a valid key; the `keys` module is the central const list for the
  shared code suite. New families slot in beside `cyclomatic`, `halstead.*`, etc.
- **Accumulator pattern.** A shared metric adds a `FooStats` struct in `mehen-metrics` with
  `record_*` (observe a node), `finalize_minmax` (snapshot per-space), and `merge` (fold child
  into parent), wired through `State` and `apply_state_to` (`crates/mehen-metrics/src/state.rs`).
  The per-language crate only decides *which AST nodes trigger `record_*`*; the math and rollup
  are shared. This is the target shape for Category 1.
- **Selectors and polarity.** `MetricSelector` supports `.min/.max/.avg/.sum` aggregators and
  `Polarity::{HigherIsWorse, HigherIsBetter}` (`selector.rs`, `threshold.rs`) — new metrics should
  declare polarity so thresholds and top-offender ranking work.
- **Unused explainability primitive.** `MetricContribution` + `ContributionReason` (span + reason
  code) exist in `analysis.rs` but are largely unpopulated. Any new *composite risk* metric is a
  natural first consumer — emit a contribution per increment so `mehen diff` can explain "why."
- **`mehen-git` is diff-only today.** `open_repo`, `changed_files`, `read_blob`,
  `friendly_ref_label`. Category 4 requires walking commit history — new capability, but it stays
  fully deterministic given a fixed repository state.

---

## 3. Category 1 — Static structural heuristics

Deterministic, computed from a single file's AST/CST. This is mehen's sweet spot.

### 3.1 DepDegree (data-flow dependency degree)

> **Measures:** total number of edges in a function's use-def (data-flow) graph — for each
> operation, the count of reaching definitions it depends on, summed over all operations:
> `dd(G) = Σ_{b∈B} dd_G(b) = |E|`.
> **Primary source:** Beyer & Fararooy, *DepDegree: A Software Metric for the Complexity of
> Programs*, ICPC 2010 ([DOI](https://doi.org/10.1109/icpc.2010.49)); formal validation in Beyer &
> Häring 2014 ([DOI](https://doi.org/10.1145/2597008.2597794),
> [project page](https://www.sosy-lab.org/research/DepDegreeProperties/)).
> **Single-file?** Yes (intra-procedural). **Deterministic?** Yes.
> **External deps / trained artifact:** none — but needs intra-procedural reaching-definitions
> data-flow analysis, which is *beyond* plain AST traversal.
> **Effort:** **M/L**. **Verification:** definition 3-0 unanimous.

**Why it's interesting for mehen.** DepDegree discriminates complexity that mehen's current metrics
cannot. The canonical example: two functionally equivalent variable-swap implementations both have
cyclomatic complexity 1 and statement count 3, but score DepDegree 6 vs 3 — capturing that a
temp-variable swap threads more data dependencies than a tuple swap. It was formally validated
against *all* of Weyuker's properties.

**Caveat (verified).** The claim that DepDegree is empirically validated as a *good readability /
understandability predictor* was **refuted** in verification (1-2 vote): the original paper's
supporting experiments are explicitly "preliminary." Treat DepDegree as a theoretically grounded
structural discriminator, **not** a proven readability predictor.

**Implementation note.** mehen already builds a full CST per space. DepDegree needs a
reaching-definitions pass over that CST per function — assign each identifier a def/use role,
compute reaching defs (a standard forward data-flow fixpoint), and count edges. The intra-procedural
scope keeps it single-file. Per-language cost is in the def/use classification (which nodes bind vs
read a variable), analogous to how each language already classifies Halstead operators/operands.

### 3.2 Shannon textual entropy and structural (AST-edge) entropy

> **Measures:** `H_TOKEN = −Σ p(word)·log₂ p(word)` over token/word frequencies in the file, and
> `H_AST_EDGE = −Σ p(edge)·log₂ p(edge)` over AST parent→child edge-type frequencies. Plain
> base-2 Shannon entropy over *empirical relative frequencies within the file*.
> **Primary source:** Torres, Baltes, Treude & Wagner, *On the Entropy of Source Code*, Empirical
> Software Engineering 2025 ([Springer](https://link.springer.com/article/10.1007/s10664-025-10644-y),
> [arXiv](https://arxiv.org/abs/2506.06508)); NLBSE'23 precursor.
> **Single-file?** Yes. **Deterministic?** Yes.
> **External deps / trained artifact:** **none** — this is the key contrast with Hindle-style
> cross-entropy (§5). `H_TOKEN` needs only a lexer; `H_AST_EDGE` needs only the AST mehen already
> builds.
> **Effort:** **S/M**. **Verification:** definition + non-redundancy 3-0 unanimous.

**Why it's the strongest new deterministic candidate.** The 2025 study measured entropy's
correlation with the metrics mehen already computes and found it **non-redundant**: correlation with
McCabe cyclomatic complexity is only −0.05 to 0.32, and correlations with nloc, token count, and
changed-methods are weak. The authors conclude "entropy may capture dimensions of complexity not
measured by classic definitions." It requires no new parsing infrastructure and slots directly into
the accumulator pattern (accumulate a frequency map per space, finalize to an entropy value).

**Caveats (verified).** (1) The corpus is **Java-only** (95 projects, 1.8M change events);
cross-language generalization to Kotlin/TypeScript/PHP/SQL is unconfirmed. (2) The authors do *not*
claim construct validity against "true" complexity — only statistical non-redundancy, which is
exactly what matters for adding a complementary signal. mehen should validate the
low-correlation-with-CC property on its own corpora per language before promoting it past
experimental (see §10).

**Design decision to make.** `H_AST_EDGE` requires choosing a per-language AST-edge vocabulary and a
token-normalization scheme for `H_TOKEN` (raw tokens? identifier-folded? keyword-only?). Because
mehen is multi-language, these choices should be centralized in `mehen-metrics` with per-language
node/token classification hooks, mirroring the Halstead operator/operand split.

### 3.3 Statistical moments of indentation

> **Measures:** standard deviation (STD), variance (VAR), and per-line summation (SUM) of leading
> whitespace across lines, as a language-independent proxy for cyclomatic and Halstead complexity.
> **Primary source:** Hindle, Godfrey & Holt, *Reading Beside the Lines: Indentation as a Proxy
> for Complexity Metrics*, ICPC 2008 ([PDF](https://plg.uwaterloo.ca/~migod/papers/2008/icpc08-abram.pdf));
> extended in Science of Computer Programming 2009.
> **Single-file?** Yes. **Deterministic?** Yes.
> **External deps / trained artifact:** **none — and no parser required.** A plain line scanner
> suffices, ~2–4× cheaper than token-based Halstead, and it works on non-compilable fragments.
> **Effort:** **S**. **Verification:** 3-0 unanimous.

**Why it's a natural fit for `mehen diff`.** This is the most parser-free candidate in the survey.
Because it needs no grammar, it can produce a complexity proxy for diff hunks, unsupported
languages, or partially-parsed files — a useful floor when a real parse is unavailable.

**Caveats (verified, important for honest presentation).** (1) The paper found **AVG and MED do
*not* correlate** with any complexity metric — only STD, VAR, and SUM are useful; do not ship the
mean. (2) The correlation strength is **modest, not strong** (rank correlations ~0.4–0.6;
STD/VAR alone gave top-10 precision/recall ~0.39, *worse* than LOC's ~0.475). (3) SUM largely
restates LOC, so it adds little beyond the LOC family mehen already has. Realistically, **STD and
VAR of indentation** are the defensible additions, positioned as a cheap parser-free proxy, not a
replacement for the real structural metrics.

### 3.4 Gaps within Category 1 (no confirmed candidate found)

The research pipeline surfaced **no** verified, reproducible, post-2015 candidate for three
sub-areas the survey specifically sought:

- **Cognitive-complexity refinements.** No novel post-2015 refinement of SonarSource cognitive
  complexity survived verification. The SonarSource model mehen already implements remains the
  reference; critiques found (Lavazza 2022; Frontiers EEG studies) attack its *predictive validity*,
  not its determinism or specification. **No action** beyond the existing implementation.
- **Newer coupling / cohesion / fan-in / fan-out heuristics.** No verified single-file candidate
  emerged. The classic CK suite (CBO, LCOM, RFC, DIT, NOC) remains the reference, but most of it
  needs whole-program (cross-file) resolution, so it belongs with a future project-scope analysis
  layer, not the single-file model. A *within-file* LCOM (method↔field access matrix) is
  computable single-file and would be a reasonable independent proposal, but no recent research
  motivated it in this survey.
- **API-usage complexity.** No verified reproducible definition surfaced.

These are documented as open questions in §10 rather than proposed here, to keep this document to
metrics with a clear implementable definition.

---

## 4. Category 2 — Learned readability / understandability

These score readability from features trained on human ratings. The decisive question for mehen is
**whether a metric ships reproducible coefficients** (deterministic at inference) **or requires a
trained classifier artifact** (a model blob, retraining, non-portable).

### 4.1 Posnett, Hindle & Devanbu — "A Simpler Model of Software Readability" (the portable one)

> **Measures:** a readability probability from a 3-feature logistic model with **published
> coefficients**: `z = 8.87 − 0.033·V + 0.40·Lines − 1.5·Entropy` (V = Halstead Volume, Lines =
> line count, Entropy = byte-level Shannon entropy), `score = 1/(1+e^{−z})`.
> **Primary source:** Posnett, Hindle & Devanbu, MSR 2011
> ([PDF](https://softwareprocess.es/z/ruse-camera-ready.pdf),
> [DOI](https://doi.org/10.1145/1985441.1985454)).
> **Single-file?** Yes. **Deterministic?** **Yes at inference** — coefficients are fixed and
> published (unlike Buse/Weimer and Scalabrino).
> **External deps / trained artifact:** none at inference. Inputs are all things mehen either has
> (Halstead Volume) or can compute trivially (line count, byte entropy).
> **Effort:** **S/M**. **Verification:** 3-0 unanimous.

**Why it's the one to adopt if any.** It is the only learned readability score in the survey that is
deterministic and portable out of the box — mehen already computes Halstead Volume, and the other
two inputs are near-free. It composes cleanly with the entropy work in §3.2.

**Critical caveat (verified).** It was trained and validated **only on tiny 4–11 line snippets that
do not span function boundaries**, and the authors explicitly warn it "may very well fail to
classify correctly at a larger size." mehen is a per-file tool, so applying it at file scope is
outside its validated envelope. **Mitigation:** compute it **per function** within a file (mehen's
space model already isolates function spaces), keeping each evaluation near the snippet size the
model was fit on, and treat the file-level value as an average/min over functions rather than a
whole-file score.

### 4.2 Buse & Weimer — "Learning a Metric for Code Readability"

> **Measures:** a binary readable/unreadable probability from a trained classifier over ~20
> statically-extractable local features (line length, identifier count/length, indentation,
> keywords, comments, blank lines), each as a per-line average or maximum.
> **Primary source:** Buse & Weimer, IEEE TSE 2010
> ([preprint PDF](https://web.eecs.umich.edu/~weimerw/p/weimer-tse2010-readability-preprint.pdf),
> [DOI](https://doi.org/10.1109/TSE.2009.70)).
> **Single-file?** Yes (feature extraction). **Deterministic?** Feature extraction yes; the score
> is a **classifier output**.
> **External deps / trained artifact:** **requires shipping a trained model** (Weka-trained on 120
> annotators / 12,000 judgments). No public plug-in coefficient table is published.
> **Effort:** **L** (feature extraction M + train/ship/version a model). **Verification:** 3-0
> unanimous.

**Assessment.** The feature set is attractive and single-file, and the model reportedly predicts
human judgments ~80% of the time (better than an average individual human). But adopting it means
**shipping and versioning a trained artifact** — a departure from mehen's dependency-light,
formula-driven design. If mehen ever wants a readability score with more features than Posnett,
the pragmatic path is to **re-fit a logistic regression on Buse & Weimer's feature set and publish
the coefficients** (making it Posnett-like and deterministic), rather than shipping a Weka model.

### 4.3 Scalabrino et al. — "A Comprehensive Model for Code Readability"

> **Measures:** readability from combined **structural + textual** features, notably comment-code
> coherence / textual coherence; reported ~84.4% accuracy, significantly higher than Buse & Weimer
> (~77.1%), Posnett (~71.5%), and Dorn (~78.8%).
> **Primary source:** Scalabrino, Linares-Vásquez, Poshyvanyk & Oliveto, ICPC 2016 / JSEP 2018
> ([PDF](https://sscalabrino.github.io/files/2018/JSEP2018AComprehensiveModel.pdf),
> [DOI](https://doi.org/10.1002/smr.1958)).
> **Single-file?** Yes. **Deterministic?** Feature extraction yes; the score is a **classifier
> output**.
> **External deps / trained artifact:** **requires shipping trained LR weights** *plus* NLP-style
> textual-feature machinery (tokenization of comments/identifiers, coherence computation).
> **Effort:** **L/XL**. **Verification:** 3-0 unanimous.

**Assessment.** The most accurate readability model in the survey, and its textual-coherence idea
(do comments describe the code they sit beside?) is genuinely novel relative to mehen's purely
structural code metrics. But it is the heaviest to adopt: a trained artifact **and** NLP
dependencies. Interesting for the Markdown/prose side of mehen (which already has NLP-style prose
metrics) more than for the code suite.

### 4.4 The understandability caveat (governs how §4 is presented)

> **Finding (verified 3-0):** Scalabrino et al., IEEE TSE 2019
> ([PDF](https://www.cs.wm.edu/~denys/pubs/TSE%2719-Understandability.pdf),
> [DOI](https://doi.org/10.1109/tse.2019.2901468)) tested 121 metrics against 444 human
> understandability evaluations and found a "bold negative result": **none** correlated
> significantly with perceived or actual understandability, and combining them into
> classification/regression models yielded only modest improvement (best classifier misclassifies
> ~33%). Lavazza et al. (2022/2023) corroborate that structural measures alone — including
> Cognitive Complexity — cannot build an accurate understandability model (~30% error).

**Implication.** Readability and understandability are *distinct constructs*; readability models
predict readability judgments, not comprehension. mehen must **not brand any single metric — or a
modest combination — as an "understandability" or "comprehensibility" score.** Label these
"readability (proxy)" or fold them into review-prioritization, with an explicit caveat in the docs.

---

## 5. Category 3 — Code naturalness / entropy

### 5.1 Hindle cross-entropy ("naturalness")

> **Measures:** the cross-entropy of a file's token sequence under a **pre-trained n-gram language
> model**: `H_M(s) = −(1/n)·log p_M(a₁…aₙ)`. Lower cross-entropy = more predictable / "natural"
> code; anomalously high entropy flags "surprising" code.
> **Primary source:** Hindle, Barr, Su, Gabel & Devanbu, *On the Naturalness of Software*, ICSE
> 2012 ([PDF](https://softwareprocess.es/pubs/hindle2012ICSE.pdf)); bug link in Ray et al. 2016;
> tooling in [SLP-Core](https://github.com/SLP-team/SLP-Core).
> **Single-file?** **No** — needs a corpus-trained model. **Deterministic?** Only once the model
> `M` is frozen.
> **External deps / trained artifact:** a **pre-trained, per-language n-gram model** (authors use
> Modified Kneser-Ney smoothing) applied to comment-stripped, **lexically analyzed** token
> sequences — i.e., a trained artifact *plus* a per-language lexer. Even "self cross-entropy" uses
> 10-fold cross-validation, so a corpus is mandatory.
> **Effort:** **L/XL**. **Verification:** 3-0 unanimous.

**Why it's compelling yet a poor fit.** Naturalness is genuinely predictive — Ray et al. (2016)
showed buggy lines are measurably less "natural" and become more natural once fixed, and SLP-Core's
cache language models exploit code "localness." But computing it **conflicts directly with mehen's
two core constraints**: it is not single-file (needs a corpus model), and it is only deterministic
once a specific model artifact is frozen and bundled per language — which also raises "deterministic
across platforms" and versioning concerns.

**If mehen ever pursues this**, the only architecture that preserves determinism is to **train a
fixed per-language model offline, version it, and bundle it as a data artifact** (like the grammars
already vendored) — with the score computed against that frozen model. This is a large,
standalone project, not an incremental metric. The §3.2 Shannon entropies are the **deterministic,
no-model way to capture "the entropy dimension"** and should be preferred first; naturalness is a
later, heavier option if the entropy signal proves valuable.

---

## 6. Category 4 — Git / history process metrics (highest-value gap)

**This is where mehen is most incomplete and where the fit is best.** `mehen-git` currently only
determines *which* files changed for `mehen diff`. Every metric below is deterministic given a fixed
repository state, language-agnostic, and cheap. The empirical case is strong (§7): process metrics
out-predict static code metrics for defects, and they cost roughly an order of magnitude less to
compute.

> **Verification honesty note.** These Category 4 claims were extracted from **primary sources**
> (code-maat, PyDriller, Google's ICSE 2013 paper, Nagappan & Ball TSE 2005, CodeScene docs) but
> did **not** pass through the 3-vote adversarial gate — the verification budget (25 claims) was
> exhausted by Categories 1–3. The *formulas* below are quoted from those primary sources; their
> *definitional* accuracy is high-confidence, but they carry a lighter verification stamp than §§3–5.
> A dedicated verification pass is recommended (§10).

### 6.1 Code churn

> **Measures:** amount of change to a file over a period. Two selectable variants (PyDriller):
> `(added − removed)` or `(added + removed)` lines, summed across commits; exposed as total / max /
> avg per file. **Nagappan & Ball** show *relative* churn (churn normalized to file size / temporal
> extent) is the defect-predictive form; *absolute* churn is a poor predictor.
> **Primary sources:** [PyDriller process metrics](https://pydriller.readthedocs.io/en/latest/processmetrics.html);
> Nagappan & Ball, *Use of Relative Code Churn Measures to Predict System Defect Density*, ICSE 2005
> ([DOI](https://dl.acm.org/doi/10.1145/1062455.1062514)); code-maat `abs-churn`.
> **Single-file?** Needs commit history for that file. **Deterministic?** Yes.
> **External deps / trained artifact:** a git repository + diff parsing; no model.
> **Effort:** **M** (churn itself) once the history-walk subsystem exists.

Ship **both** `history.churn.abs` (added+removed) and `history.churn.relative`
(churn ÷ current size) — the research is explicit that the relative form is the one with defect
signal, while absolute churn is easier to compute and matches code-maat's default.

### 6.2 Code age

> **Measures:** months since a module's last change (configurable "time zero"); a proxy for
> stability (recently-churned code is riskier; long-stable code is settled).
> **Primary source:** code-maat `age` analysis
> ([repo](https://github.com/adamtornhill/code-maat)); Tornhill, *Your Code as a Crime Scene*.
> **Single-file?** Needs the file's last-commit date. **Deterministic?** Yes (given fixed "now").
> **External deps / trained artifact:** git; no model. **Effort:** **S**.

Note the **determinism wrinkle**: age depends on "now." mehen should default "time zero" to the
repository HEAD commit date (not wall-clock time) so results are reproducible across runs and
machines — matching mehen's cross-platform determinism contract.

### 6.3 Ownership / authorship metrics

> **Measures:** per file — number of distinct authors; **minor contributors** (developers
> contributing < 5% of lines); **contributors experience** (% of lines authored by the single
> top contributor); main developer.
> **Primary sources:** [PyDriller process metrics](https://pydriller.readthedocs.io/en/latest/processmetrics.html)
> (fixed 5% minor-contributor threshold); code-maat `authors`, `main-dev`, `entity-ownership`.
> **Single-file?** Needs `git blame` / commit authorship for that file. **Deterministic?** Yes.
> **External deps / trained artifact:** git; no model. **Effort:** **M**.

Number-of-authors is one of the most-validated defect signals in the literature (Tornhill:
"number-of-authors … [is a] validated predictor of post-release defects"). The fixed thresholds
(5% minor contributor) are concrete and reproducible.

### 6.4 Change coupling / temporal coupling

> **Measures:** how often two files change in the same commit. **Degree of coupling** = % of shared
> revisions two files change together. **Sum of Coupling (SoC)** = per-file aggregate of how often a
> file co-changes with *any* other file — a single-number architectural-significance signal.
> **Primary sources:** code-maat `coupling` / `soc`
> ([repo](https://github.com/adamtornhill/code-maat)); CodeScene temporal-coupling docs.
> **Single-file?** **No** — inherently pairwise / repository-scope. **Deterministic?** Yes.
> **External deps / trained artifact:** git; no model. **Effort:** **L** (pairwise co-change over
> history; needs noise thresholds).
> **Reproducible thresholds (code-maat / CodeScene defaults):** ignore changesets > 30–50 files;
> ignore couples < 30–50% strength; require ≥ 5–10 shared commits; require ≥ 10 revisions/file;
> exclude couples explained only by a shared creation commit.

This is the metric that most needs the **top-offender/report layer** rather than the per-file
metric set, because its output is a *ranking of file pairs* (or SoC per file). It is also the most
implementation-heavy Category 4 item. **SoC is the pragmatic first step** — it collapses coupling to
one number per file, which fits mehen's existing per-file, top-offender model.

### 6.5 Hotspots (complexity × change frequency)

> **Measures:** files where a complexity proxy (LOC, or one of mehen's real complexity metrics) and
> change frequency (commit count) **overlap** — the highest-leverage refactoring targets.
> **Primary source:** CodeScene hotspots docs; Tornhill, *Your Code as a Crime Scene* /
> *Software Design X-Rays*.
> **Single-file?** Combines a single-file metric with that file's commit frequency.
> **Deterministic?** Yes (for the open, LOC×frequency form). **External deps:** git; no model.
> **Effort:** **S** once churn/frequency exists (it's a product of two values mehen would already
> have). **Verification:** CodeScene's *ranking/prioritization* layer is **proprietary and
> probabilistic** — do not attempt to replicate it. The open `complexity × change-frequency`
> overlap is reproducible; the ranked "refactoring targets" are not.

**Strong recommendation.** A hotspot signal is nearly free once §6.1/§6.3 exist, and it is the most
*actionable* history metric — CodeScene reports that top hotspots occupy ~5.5% of code yet absorb
~17.6% of effort and ~23% of fixed defects. Because mehen has *real* complexity metrics (cognitive,
cyclomatic), it can compute a **better hotspot than the LOC-based default** — e.g., `cognitive.sum ×
history.commit_frequency`. This is a compelling composite that also lights up the unused
`MetricContribution` primitive.

### 6.6 Time-Weighted Risk (Google bug-prediction)

> **Measures:** a per-file bug-propensity score summing a logistic time-decay weight over the file's
> bug-fixing commits: `Σᵢ 1/(1 + e^{−12·tᵢ + ω})`, where `tᵢ` is the commit time normalized to
> [0,1] and `ω` tunes the decay window (~6–8 months at ω hard-coded to 12).
> **Primary source:** Lewis et al., *Does Bug Prediction Support Human Developers?*, ICSE 2013
> ([PDF](https://users.soe.ucsc.edu/~ejw/papers/lewis-icse-2013.pdf)).
> **Single-file?** Needs the file's bug-fixing commit history. **Deterministic?** Yes (given a rule
> for identifying bug-fixing commits).
> **External deps / trained artifact:** git + a bug-fix commit classifier (e.g., message regex
> `fix|bug|#\d+`); **no trained model**. **Effort:** **M**.

**Two honest caveats (both from the primary source).** (1) The score needs a *definition of
"bug-fixing commit"* — mehen would use a configurable message heuristic, which is a source of noise.
(2) The simplest variant — **the "Rahman algorithm," just ranking files by count of bug-fixing
commits** — performed almost as well as more complex schemes and was *preferred by Google developers
for transparency*. So mehen should offer `history.bugfix_commits` (trivial, transparent) first, and
TWR as a decayed refinement. Note also that Google's own deployment produced **no significant change
in developer behavior** — a signal's existence doesn't guarantee it changes outcomes; present it
modestly.

### 6.7 Secondary history heuristics (from PyDriller)

- **Hunks count** — median number of contiguous diff blocks touching a file; a change-fragmentation
  signal (scattered edits vs one localized change). Deterministic; **S**.
- **Change set** (max/avg files committed together) — a repository-scope co-change signal, cheaper
  than full pairwise coupling. Deterministic; **S**.
- **Commits count**, **lines count** (total added/removed over history) — trivial history rollups; **S**.

### 6.8 Licensing constraint (must-read before implementing)

**code-maat is GPL-v3** and its analyses evolved into the **proprietary CodeScene** product. mehen
may **reimplement the open, reproducible formulas** (all quoted above are published in Tornhill's
books and the code-maat README), but must **not copy code-maat's Clojure source** into a
permissively-licensed Rust CLI. CodeScene's *prioritization/ranking algorithms* are proprietary and
not reproducible — implement only the open overlap/coupling definitions. PyDriller (Apache-2.0) and
the academic formulas (Nagappan-Ball, Google TWR) are safe references.

---

## 7. Empirical reality check (why, and why-not)

The survey's validation-focused sources converge on a nuanced picture mehen should encode in how it
*presents* metrics:

1. **Process/history metrics beat static code metrics for defect prediction.** Rahman & Devanbu
   (2013) and Bal & Kumar / large-scale replication (EMSE 2022, 722k commits / 700 projects): best
   process learners reach ~98% recall / 95% AUC vs ~44% / ~54% for product (code) learners; process
   metrics are also ~10× cheaper and language-agnostic. **→ Strong argument for Category 4.**
2. **No single metric captures understandability** (Scalabrino TSE 2019; Lavazza 2022). **→ Never
   brand any score "understandability."** (§4.4)
3. **Plain LOC predicts faults about as well as complexity metrics; combined metric sets do best**
   (Hall et al.; Radjenović et al. 2013 SLR). **→ mehen's value is breadth + combination, not any
   single hero metric. Add complementary families (entropy, history) rather than more
   complexity variants.**
4. **Metric-importance rankings don't generalize across scales** (EMSE 2022). **→ Don't hard-code
   weights from small studies; keep composites configurable and explainable.**

---

## 8. Proposed prioritization for mehen

| Tier | Candidate | Category | Effort | Deterministic | Ships a model? | Rationale |
|---|---|---|---|---|---|---|
| **1 — do first** | Shannon `H_TOKEN` + `H_AST_EDGE` | Static | S/M | ✅ | ❌ | No new deps; empirically non-redundant with CC; pure fit. |
| **1** | History: churn (abs+relative), code age, author count, commits count | History | M (subsystem) | ✅ | ❌ | Biggest gap; best defect signal; matches diff/top-offender model. |
| **1** | Hotspot = `cognitive.sum × commit_frequency` | History composite | S* | ✅ | ❌ | Nearly free after churn; most actionable; first `MetricContribution` consumer. |
| **2 — high value, more work** | DepDegree | Static | M/L | ✅ | ❌ | Discriminates what CC/statement-count miss; needs data-flow pass. |
| **2** | Change coupling / Sum-of-Coupling | History | L | ✅ | ❌ | Powerful but pairwise/repo-scope; start with SoC. |
| **2** | Time-Weighted Risk (+ transparent `bugfix_commits`) | History | M | ✅ | ❌ | Needs bug-fix commit heuristic; ship the transparent count first. |
| **3 — deterministic, lower payoff** | Indentation STD/VAR | Static | S | ✅ | ❌ | Parser-free proxy; only *modest* correlation; drop AVG/MED and SUM. |
| **3** | Posnett readability (per-function) | Learned | S/M | ✅ | ❌ | Only portable learned model; keep inside its 4–11-line size envelope. |
| **4 — heavy / poor fit** | Buse-Weimer / Scalabrino readability | Learned | L/XL | ⚠️ (model) | ✅ | Require shipped trained artifacts (+NLP for Scalabrino). |
| **4** | Hindle naturalness | Naturalness | L/XL | ⚠️ (frozen model) | ✅ | Not single-file; needs bundled per-language n-gram model. |

`*` Hotspot effort is S *given* the Tier-1 history subsystem.

**Suggested namespaces** (open `MetricKey` space, following existing `family.subkey` convention):
`entropy.token`, `entropy.ast_edge`; `depdegree` (+ `.sum/.avg/.max`); `indent.std`, `indent.var`;
`readability.posnett`; and a new **`history.*`** family — `history.churn.abs`,
`history.churn.relative`, `history.age_months`, `history.authors`, `history.minor_contributors`,
`history.ownership`, `history.commit_frequency`, `history.hotspot`, `history.sum_of_coupling`,
`history.twr`, `history.bugfix_commits`. Declare `Polarity` per key (most are `HigherIsWorse`;
`history.age_months` and `history.ownership` are `HigherIsBetter`).

---

## 9. Recommendation for the GitHub Action comment default set

**Question:** if we want to replace the current default metrics shown in the GitHub Action
(PR-comment) table — or add just one — what should it be?

### 9.1 What the comment shows today, and why it's redundant

The PR comment is rendered by `mehen diff`'s Markdown table, one column per default selector. The
default set is a single source of truth in `crates/mehen-engine/src/metric_selector.rs`
(`DEFAULT_METRICS`, consumed by `run_diff` → `default_selectors_for_language` → `print_markdown`):

| # | Selector | Label | Underlying key | Dimension |
|---|---|---|---|---|
| 1 | `cyclomatic` | Cyclomatic | `cyclomatic.sum` | control-flow complexity |
| 2 | `cognitive` | Cognitive | `cognitive.sum` | control-flow complexity |
| 3 | `nom.functions` | Functions | `nom.functions` | size (count) |
| 4 | `loc.lloc` | LLOC | `loc.lloc` | size (lines) |
| 5 | `mi.visual_studio` | MI | `mi.visual_studio` | **composite of 1 + Halstead volume + SLOC** |

(SQL files use a disjoint `DEFAULT_SQL_METRICS` set — the analysis below is about the source-code
default; the same reasoning was already applied to give SQL its own composite-led defaults.)

Two structural problems:

1. **The five columns collapse to two dimensions plus a composite that double-counts them.**
   Cyclomatic and cognitive are both control-flow; `nom.functions` and `loc.lloc` are both size; and
   `mi.visual_studio` is *defined as* `max(0, (171 − 5.2·ln(V) − 0.23·Cyclomatic − 16.2·ln(SLOC))·100/171)`
   — so it re-encodes cyclomatic (already column 1) and size (already columns 3–4). Real
   information density is closer to **2.5 columns, not 5.** The **ABC** magnitude — whose
   *assignments* term is a data-manipulation/computation-volume axis — and the **Halstead
   difficulty/effort** vocabulary axis are entirely absent, even though both are already computed
   for every language.

2. **A diff comment shows no change-relevant signal.** The table lists absolute per-file metric
   values. The question a reviewer actually asks — *"is this a risky change to an already-fragile
   file?"* — needs a **history** column (§6), which mehen cannot yet produce. This is the strongest
   argument that the highest-value *comment* improvement is gated on the Category-4 work, not on the
   static suite.

### 9.2 If adding exactly one column (zero-to-low effort)

**Recommendation: add `abc` (ABC magnitude).** It is already implemented for every language, already
in `KNOWN_METRICS`, and — crucially — measures a dimension none of the five current columns capture:
raw computational volume (Assignments/Branches/Conditions). A function can be cyclomatically flat and
short yet have a large ABC because it does a lot of straight-line work; that is exactly the
"deceptively heavy change" the current table hides.

> **One-line change:** append `"abc"` to `DEFAULT_METRICS` in
> `crates/mehen-engine/src/metric_selector.rs`. Effort **S** (plus golden-snapshot updates for the
> Markdown/JSON reporters). No new computation, no new dependency.

*Alternative if a truly novel signal is preferred over an existing one:* `entropy.token` (§3.2) once
implemented — it is the only static addition the research showed to be **non-redundant with
cyclomatic complexity** (correlation −0.05 to 0.32). Prefer this once §3.2 lands; until then, `abc`
is the free win.

### 9.3 If replacing the set (recommended target)

Design the comment around **one column per orthogonal dimension**, dropping the redundancy. A
principled 5-column set, in order:

| Column | Selector | Dimension it uniquely covers | Status |
|---|---|---|---|
| Cognitive | `cognitive` | control-flow *understandability* (keep the better of the two flow metrics) | ✅ selectable |
| ABC | `abc` | computational volume (assignments/branches/conditions) | ✅ selectable |
| Halstead effort | `halstead.effort` | vocabulary/operator burden | ⚠️ computed but **not yet selectable** — see note |
| LLOC | `loc.lloc` | size | ✅ selectable |
| MI | `mi.visual_studio` | at-a-glance rollup (kept as the one deliberate composite) | ✅ selectable |

Rationale: **drop `cyclomatic`** (cognitive is the more defensible flow metric and the two correlate
strongly) and **drop `nom.functions`** (LLOC already carries size; function *count* is weak signal in
a diff). Spend the freed columns on the two orthogonal axes that were missing — ABC and Halstead
effort.

**Wiring caveat (one metric is not free).** Four of these five (`cognitive`, `abc`, `loc.lloc`,
`mi.visual_studio`) are already registered as selectors in
`crates/mehen-engine/src/metric_selector.rs` (present in `KNOWN_METRICS` and mapped in
`metric_set_key_for`), so for them this is a **pure-config change** to `DEFAULT_METRICS` plus
reporter snapshots. **`halstead.effort` is the exception:** the walker *publishes* the value
(`crates/mehen-metrics/src/state.rs` emits the `halstead.effort` key), but only `halstead.volume` is
registered as a *selector* today, and `halstead.effort` is not a namespaced (`sql.*`/`markdown.*`)
key, so it would fall through to "Unknown metric, skipping" rather than render. Adding it needs two
one-line registrations first — a `("halstead.effort", "Halstead Effort", Polarity::LowerIsBetter)`
entry in `KNOWN_METRICS` and a `"halstead.effort" => "halstead.effort"` arm in `metric_set_key_for`.
So §9.3 is **"no new metric *formula*"**, not "no code": budget the small selector-registration
change for the Halstead effort column. Effort **S**.

### 9.4 The strategic answer (once history lands)

The most valuable comment is not a better static column — it is a **change-risk column** the current
architecture cannot yet emit. Target end-state for the PR comment, after §6 Tier-1 work:

| Column | Selector | Why it belongs in a *diff* comment |
|---|---|---|
| Cognitive | `cognitive` | how hard the changed code is to follow |
| ABC | `abc` | how much the change actually computes |
| MI | `mi.visual_studio` | at-a-glance maintainability rollup |
| **Hotspot** | **`history.hotspot`** | **`cognitive.sum × commit_frequency` — is this a fragile, frequently-touched file?** |
| **Churn** | **`history.churn.relative`** | **how much of the file this change moves, size-normalized** |

The two **bold** columns are the ones that make a *diff* comment answer the reviewer's real question,
and they are precisely what §6 proposes building. **Sequencing recommendation:** ship §9.3 now as a
pure-config cleanup (removes redundancy, adds two orthogonal axes at zero metric cost), then extend
the default set with `history.hotspot` + `history.churn.relative` when the Category-4 history layer
exists. Note the per-language default mechanism (`default_metrics_for_language`) already supports
this — history columns would be added to the shared default, while SQL keeps its own set.

### 9.5 Caveat carried from §7

Whatever the comment shows, label the columns as **review-prioritization signals**, not quality
verdicts (§4.4/§7). A rising cognitive or hotspot number flags *where to look*, not *that the code is
bad* — the empirical literature is explicit that no single metric certifies (un)maintainability.

---

## 10. Open questions / recommended follow-up research

1. **Dedicated Category 4 verification + design pass.** The history metrics are the highest-value
   addition but carry the lightest verification stamp in this document (§6 note). Run a focused
   research + verification pass on churn/age/ownership/coupling/hotspot/TWR definitions and reference
   implementations (code-maat, PyDriller, CodeScene open docs), then write a `mehen-git`
   history-layer design doc (repository walk, commit caching, determinism via HEAD-relative "now",
   bug-fix commit heuristic configuration).
2. **Cross-language entropy validation.** Does Torres et al.'s low-correlation-with-CC
   (non-redundancy) result hold for Kotlin/TypeScript/PHP/SQL/Markdown, and what per-language
   AST-edge vocabulary and token normalization should `H_AST_EDGE` / `H_TOKEN` use? (§3.2)
3. **Posnett at file scope.** Can the 3-feature model be safely applied per-function within a file,
   or should it be recalibrated for larger units? Validate before shipping. (§4.1)
4. **Within-file LCOM and API-usage complexity.** No recent research surfaced, but a single-file
   LCOM (method↔field access matrix) and an API-fan-out count are plausible independent proposals;
   worth a targeted search distinct from this survey. (§3.4)

---

## 11. Sources

All primary sources below were fetched and, except where §6 notes otherwise, their definitional
claims passed 3-0 adversarial verification.

**Static structural**
- Beyer & Fararooy, *DepDegree*, ICPC 2010 — https://doi.org/10.1109/icpc.2010.49
- Beyer & Häring, *DepDegree properties* (Weyuker validation), 2014 — https://www.sosy-lab.org/research/DepDegreeProperties/
- Torres, Baltes, Treude & Wagner, *On the Entropy of Source Code*, EMSE 2025 — https://link.springer.com/article/10.1007/s10664-025-10644-y · https://arxiv.org/abs/2506.06508
- Hindle, Godfrey & Holt, *Reading Beside the Lines*, ICPC 2008 — https://plg.uwaterloo.ca/~migod/papers/2008/icpc08-abram.pdf
- SonarSource, *Cognitive Complexity* white paper (archetype; already implemented) — https://www.sonarsource.com/docs/CognitiveComplexity.pdf

**Learned readability / understandability**
- Posnett, Hindle & Devanbu, *A Simpler Model of Software Readability*, MSR 2011 — https://softwareprocess.es/z/ruse-camera-ready.pdf
- Buse & Weimer, *Learning a Metric for Code Readability*, IEEE TSE 2010 — https://web.eecs.umich.edu/~weimerw/p/weimer-tse2010-readability-preprint.pdf
- Scalabrino et al., *A Comprehensive Model for Code Readability*, JSEP 2018 — https://sscalabrino.github.io/files/2018/JSEP2018AComprehensiveModel.pdf
- Scalabrino et al., *Automatically Assessing Code Understandability*, IEEE TSE 2019 — https://www.cs.wm.edu/~denys/pubs/TSE%2719-Understandability.pdf

**Naturalness / entropy**
- Hindle et al., *On the Naturalness of Software*, ICSE 2012 — https://softwareprocess.es/pubs/hindle2012ICSE.pdf
- Ray et al., *On the "Naturalness" of Buggy Code*, ICSE 2016 — https://arxiv.org/abs/1506.01159
- SLP-Core (reference implementation) — https://github.com/SLP-team/SLP-Core

**Git / history process (primary-sourced; see §6 verification note)**
- code-maat (Adam Tornhill; GPL-v3) — https://github.com/adamtornhill/code-maat
- PyDriller process metrics (Apache-2.0) — https://pydriller.readthedocs.io/en/latest/processmetrics.html
- Lewis et al., *Does Bug Prediction Support Human Developers?* (Google TWR), ICSE 2013 — https://users.soe.ucsc.edu/~ejw/papers/lewis-icse-2013.pdf
- Nagappan & Ball, *Relative Code Churn*, ICSE 2005 — https://dl.acm.org/doi/10.1145/1062455.1062514
- CodeScene hotspots / temporal-coupling docs — https://docs.enterprise.codescene.io/

**Empirical validation / SLRs**
- Radjenović et al., *Software fault prediction metrics: A systematic literature review*, IST 2013
- Rahman & Devanbu, *How, and why, process metrics are better*, ICSE 2013
- Large-scale replication (process vs product, 700 projects), EMSE 2022 — https://arxiv.org/abs/2008.09569
- Hall et al., *A systematic review of fault prediction performance* — http://crest.cs.ucl.ac.uk/cow/15/HallBBGC2011.pdf

---

## 12. Provenance

This document was produced by (a) an exhaustive read-only catalog of mehen's current metric
inventory, and (b) a deep-research pipeline that decomposed the question into 5 angles, ran parallel
web searches (Exa/Tavily), fetched 26 primary sources, extracted 127 falsifiable claims, and put the
top 25 through 3-vote adversarial verification (24 confirmed, 1 refuted — the DepDegree
predictive-validity claim in §3.1). The verification budget was consumed by Categories 1–3, so
Category 4 (§6) is primary-sourced but not adversarially voted; §10.1 recommends closing that gap.
Empirical accuracy figures (80%, 84.4%, 98% recall) are dataset-specific, and the entropy
non-redundancy result is Java-only — see the per-section caveats.
