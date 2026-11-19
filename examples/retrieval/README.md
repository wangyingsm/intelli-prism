# Retrieval against a real model, over both backends, on one corpus

Twelve pages of an invented operations handbook, embedded once by the deployment's own artefact and
written into both a `qdrant` collection and a `dr-strange` plane. Two graphs of the same two steps,
and the same two templates:

```
look ──> answer

graphs/points.json   look names docs.search                        a collection, and nothing else
graphs/joins.json    look names kb.search, kb.neighbors, kb.rank   a plane, and the joins in it
```

Neither template names a tool. **Each `look` node names its own**, which is what lets one template
serve a node that reads a collection and a node that walks a graph.

Three runs. The collection is asked something one page answers outright. The plane is asked
something whose nearest page has been **superseded**, where what to answer with is on the page
joined to it by `SUPERSEDED_BY`. Then the collection is asked that same question, holding the same
page and no join from it.

## Running it

```
. ~/.config/secrets.env                 # or wherever the key lives
export IP_API_KEY="$DEEPSEEK_API_KEY"
export IP_EMBED_ARTIFACT=.../e5-small-embed-int8
export ORT_DYLIB_PATH=.../libonnxruntime.so
examples/retrieval/run.sh               # writes out/{flat,joined,blind}.sse beside this file
```

`drsg` 2.28.0 or newer, `docker` and `uv` must be on the path. Nothing else is needed: the server,
its sqlite, its sled, the plane the graphs live in, the plane the corpus lives in and the qdrant
the same corpus lives in are all made for the run and go when it ends.

Any OpenAI-compatible provider works. `IP_BASE_URL`, `IP_MODEL` and `IP_DIALECT` override the
DeepSeek defaults, `IP_FLAT` / `IP_JOINED` replace the two questions, and `IP_PORT` /
`IP_DRSG_PORT` / `IP_QDRANT_PORT` move the three listeners.

| File | What it is |
|---|---|
| `run.sh` | stands everything up, seeds both backends, registers both retrievers, starts three runs |
| `corpus.json` | twelve pages and the seven joins between them |
| `seed.py` | embeds the corpus with the deployment's own artefact and writes it into both backends |
| `graphs/points.json` | two nodes; `look` names `docs.search` and nothing else |
| `graphs/joins.json` | the same two nodes; `look` names the plane's `search`, `neighbors` and `rank` |
| `agents/look.json` | the node that looks things up, and names no tool of its own |
| `agents/answer.json` | the node that writes the answer, and looks nothing up |
| `report.py` | reads a stream and says what was looked up and what came back |
| `sample-run/` | three real runs: each stream, each step's input and answer, every look-up, each report |

## What three real runs did

`sample-run/` is three runs against `deepseek-chat` on 2026-11-19. `sample-run/joined/report.txt`
in full — the plane, asked about a two-hour temperature excursion:

```
  look     begins
    ? kb__search           {"hits": 2, "query": "A reefer on the Rotterdam run logged two ...
    = 2 rows: reefer-telemetry 0.884, cold-chain-r6! 0.876
    ? kb__search           {"hits": 2, "query": "reefer temperature excursion out of band ...
    = 2 rows: cold-chain-r7 0.875, cold-chain-r6! 0.864
    ? kb__neighbors        {"direction": "both", "node": 129}
    = 1 row: cold-chain-r7
    ? kb__neighbors        {"direction": "both", "node": 193}
    = 2 rows: reefer-telemetry, cold-chain-r6!
  look     answered  in  4907/  460 tokens,   866 characters out
  answer   answered  in   356/  131 tokens,   529 characters out

  look-ups asked for:   4
  joins followed:       2
  superseded pages hit: cold-chain-r6, cold-chain-r6, cold-chain-r6
```

It answered: re-ice at the nearest hub first, notify within **ninety minutes**, raise an Excursion
ticket — every word of it from `cold-chain-r7`, and none from the superseded `cold-chain-r6` whose
four-hour window and form CX-11 were the nearest thing to the question. `sample-run/flat/` is the
collection answering a manifest failure from one page, invented command and all.

**The embeddings never reach the prompt.** Read any line of
`sample-run/joined/1-look-lookups.txt`: the page's own properties are there in full, and where its
384 floats would be there is `"embedding":"$vector(384 dims, omitted)"`. That marker is also the
only place a plane states a vector's width, which is why a retriever's width is *measured* at
registration rather than asked for.

## Six findings, all from the runs and not from reasoning about them

**The semantic cache answered a node that held different tools, and handed it a tool call it could
not make.** `sample-run/blind/` is the collection asked the plane's question, and its first two
look-ups are **byte-identical to the plane run's** — `kb__search`, twice, with the same rephrased
second query. That graph declares no `kb__search` at all. The closing line of the run says where it
came from: of eleven recorded turns, `semantic: 1`.

The cause is structural rather than a bad threshold. A node's turn is an ordinary request as far as
the gateway is concerned — `ip-run`'s own doc says so — so it passes the semantic cache like any
other, and what that cache asks by is the conversation's **opening text**. The tools declared
alongside it are not part of the question. Two nodes given the same prompt and different tools are
therefore one question to it, and **a higher threshold does not help**: this run was at
`nearness = 1.0`, because the two openings are not merely near, they are identical.

There is no way to turn it off, either, and that is the sharper half: the embedder a look-up needs
is configured by the `[semantic]` stanza, which exists to switch the semantic cache on. A
deployment that wants retrieval gets the cache with it.

**What the join provides is the statement of replacement, not the reach.** The tidy claim — that a
vector store cannot get to `cold-chain-r7` — is false, and this example proves it false. A hit
ceiling bounds one search, not the model: asked again in better words, *both* backends found the
page (`0.875` in each). The collection run then asserted that r7 replaced r6, which it had read
nowhere; it inferred it from two revision numbers in two titles.

What the plane did instead was read the edge. On a corpus whose successor is not helpfully called
"revision 7" that inference is unavailable and the edge is load-bearing — and across four runs of
this example a collection asked this question answered correctly twice and, the other two times,
said it could not reach the replacement and **declined to answer**. That variance is the finding:
without the join, whether the right answer comes back depends on the model's phrasing luck.

**A plane holds a vector only if it is written as one.** `{"embedding": [0.1, 0.2, …]}` is
accepted, stored, and read back as exactly that array — and indexed as nothing, so every search
over it answers an empty list. Not an error: an empty answer, which is also what a plane with no
embeddings and a query of the wrong width both look like. It has to be
`{"embedding": {"$vector": [0.1, 0.2, …]}}`, which reads back as the marker above. This cost the
first attempt at the example, and it is the one mistake here that fails silently.

**The corpus has to be embedded with the deployment's query prefix, not with a passage prefix.**
A look-up embeds the model's words with `IP_EMBED_ARTIFACT` and sends the numbers on, so anything
embedded another way is searched in another geometry — and the scores come back looking perfectly
reasonable. The gateway applies the manifest's `query_prefix` to *everything* it embeds, so
`seed.py` does too. e5 was trained with `query:` on questions and `passage:` on documents; that
asymmetry is not available here, and matching what the gateway does matters more than being right
about e5.

**A node id is rendered as a string and `neighbors` takes a number.** A hit reads
`{"id":"129",…}` and the walk wants `{"node": 129}`. This model retyped it correctly on all three
walks, so the asymmetry cost nothing here — but it is a real one, and a weaker model would spend a
turn on the refusal it gets for passing the string back as it saw it.

**The scores sit in a band 0.08 wide, so ranking is the whole signal.** Every hit in every run
scored between 0.82 and 0.90, including a cold-chain page answering a manifest question at 0.825.
A nearness threshold over a corpus like this would admit everything or nothing; what carries the
meaning is the order, which is why a retriever has a hit ceiling and not a floor.

## What it cost, and what the refusals are worth

| Run | Look-ups | Joins followed | Tokens |
|---|---|---|---|
| `flat`, the collection on a question one page answers | 2 | — | 2443/374 |
| `joined`, the plane on a superseded page | 4 | 2 | 5263/591 |
| `blind`, the collection on the same question | 4 | 0 | 3966/773 |

A look-up is cheap to ask and not cheap to read: every row lands in the conversation and is paid
for on every turn after it, which is what `max_hits` is for and why the declaration tells a model
to ask for fewer where fewer will do.

Two refusals in the `blind` run are worth reading as a design working rather than as noise. `there
is no tool called "kb__search"` was answered to the model twice, and the run **went on** — it
dropped the invented name, used `docs__search`, and answered. A look-up that cannot be made is a
tool result the model is told failed, never an error that ends the run, and the whole sentence is
given to it because a model told only "it failed" asks the same thing again.

`cost_micros` is `0` in the reports because this deployment prices no models; see `ROADMAP.md`.
