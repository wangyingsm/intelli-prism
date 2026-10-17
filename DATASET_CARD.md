# intelli-prism difficulty dataset

7439 requests in English and Chinese, each labelled with a distribution over five difficulty
classes: `easy`, `routine`, `median`, `hard`, `research`. It exists to train the classifier the
gateway routes by, and it is published because the model is not reproducible without it.

## What is in it

| | |
|---|---|
| examples | 7439 |
| English / Chinese | 3638 / 3801 |
| domains | ten, named in each language |
| label | a distribution over five classes, not one class |
| `easy` / `routine` / `median` / `hard` / `research` | 1641 / 2108 / 1798 / 1566 / 326 |

`research` is heavily under-represented at 326 examples — 4% of the set — and that shows directly
in the model's weakest class.

## How it was made

**Generated.** A local model (`qwen3.8:27b-mtp-q8_0`) was asked for requests across ten domains —
backend code, frontend code, SQL and data pipelines, devops, machine learning, data analysis,
security review, writing and translation, customer support, and everyday chat — at five difficulty
hints, in each of the two languages.

**Labelled independently.** Every generated request was then labelled **five times** by that same
27B at temperature 0.8, from a rubric describing the five classes. The five samples become the
target: a request that drew three `median` and two `hard` is stored as that distribution, not as
`median`. Training against the distribution is what teaches the model to be uncertain where the
judges were, and that uncertainty is what the gateway's margin gate reads.

**The hint never became the label.** The difficulty hint that generated a request was not shown to
the labeller and was not used as its class. **59% of rows ended up with a settled class that
disagrees with the hint they were generated from** — that figure is the evidence the labelling was
independent rather than a relabelling of what was asked for.

45% of rows were unanimous across all five samples. The rest carry genuine spread.

## The ceiling, which is published because it should be

Two different judges — `qwen3.8-flash-next` and the 27B that labelled the set — both labelled the
first 350 candidates. They settle on the same class for **275 of 350 = 79%**.

That is a ceiling: a classifier taught by one judge cannot reliably beat how well two judges agree.
Where **both** judges were unanimous across their own five samples (147 items), they agree **96%**
of the time.

The gap between 79% and 96% is what ambiguity costs. It says the disagreement lives in the
requests, not in the judges — and it is why a model scoring in the high sixties overall is closer to
the achievable limit than that number sounds. Where the judges part, they part by one class and
almost always adjacently: `routine`→`median` 23 times, `median`→`hard` 14 times.

## Two things worth saying plainly

- **It is entirely synthetic.** No user data, no scraped text, no personal information. There is
  nothing to redact, no subject to notify, and no privacy review standing between this and a
  deployment. For an enterprise gateway that is the point.
- **The ceiling is measured and published**, above. A dataset whose labelling quality is unstated
  asks you to take its model's accuracy on faith.

## What would improve it

- **Ensemble labelling.** One judge's labels cap the achievable accuracy at that judge's agreement
  with another. Labelling with two or three and keeping the combined distribution would raise the
  79% ceiling, which is the single largest available gain.
- **More `research`.** 326 examples is too few for the class where a mistake costs most. Targeted
  generation of research-grade requests is the obvious next pass.
- **Real traffic.** Everything here is generated. Even a small hand-labelled sample of genuine
  requests would say how far the synthetic distribution sits from the real one, which nobody
  currently knows.

## Licence

This project's own contribution — the generated requests, the labels, and the scripts that produce
both — is dual licensed **MIT or Apache-2.0**, matching the gateway.

Both the requests and the labels are the outputs of `Qwen3.8-27B`, which is released under
**Apache-2.0**. That licence governs the weights and says nothing about what a model run from them
produces: there is no output clause and no term on training another model from them. "May outputs
be used to train another model" is the clause that varies between licences, and here it is absent,
so the dataset is this project's to license as above.

The 350 comparison labels behind the ceiling figure came from a second judge,
`Qwen3.8-Flash-Next`, which carries the Qwen Community License 1.0 instead. **Those rows are not
published.** The number they measure is; the labels themselves are not part of this dataset, so
everything published here derives from Apache-2.0 outputs alone.

The base model the classifier fine-tunes is MIT; see the model card.

## Rebuilding it

`generate.py` writes the candidates, `softlabel.py` labels them, `compare.py` measures the ceiling,
`train.py` fits the model and its temperature, `export_onnx.py` exports and verifies the graph, and
`quantize.py` builds the quantizations and refits each one's gate. Each prints what it measured.
