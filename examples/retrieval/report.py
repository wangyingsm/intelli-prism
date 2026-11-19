#!/usr/bin/env python3
"""Reads a graph run's event stream and says what was looked up, and what it was worth.

    report.py <stream.sse> [--answers <directory>]

With `--answers`, each node's input and answer is written out, numbered in the order the nodes
finished, together with every look-up as `<n>-<node>-lookups.txt`. That is how `sample-run/` is
made.

The two things to watch for are both failures of judgement rather than of plumbing: a node that
answered **without calling a look-up at all**, which is a model answering from its own knowledge
with a retrieval tool sitting unused, and a hit whose page says `superseded` that nothing was
followed from.
"""

import json
import pathlib
import sys

#: What `ip-run` joins a source to its tool with on the wire, where a tool set writes a dot.
WIRE = "__"


def events(path):
    """Every `data:` line of the stream, with the event name it arrived under."""
    name = ""
    with open(path, encoding="utf-8") as stream:
        for line in stream:
            if line.startswith("event:"):
                name = line[6:].strip()
            elif line.startswith("data:"):
                yield name, json.loads(line[5:])


def shortened(text, most=96):
    """One line of it, short enough to read in a report.

    Cut back to the last whole word, because what is shortened here is a model's own query and
    half a word in a kept report reads as a misspelling rather than as a truncation.
    """
    text = " ".join(str(text).split())
    if len(text) <= most:
        return text
    cut = text[: most - 4]
    spaced = cut.rfind(" ")
    if spaced > most // 2:
        cut = cut[:spaced]
    return cut.rstrip() + " ..."


def rows_in(text):
    """A look-up's answer as the rows it is, and nothing where it answered in words.

    A look-up answers one json object per line, so a line that will not parse is the tool saying
    something about itself — `nothing matched`, or why the arguments were wrong.
    """
    rows = []
    for line in text.splitlines():
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            return None
    return rows


def page_in(row):
    """The page one row carries: the key the corpus gave it, and what it holds.

    Three shapes, because a row is one of three things. A plane's node carries its key beside its
    properties rather than in them; a plane's hop carries that node under `node`, with the edge it
    crossed beside it; and a collection's point is its payload and nothing else.
    """
    found = row.get("found")
    if not isinstance(found, dict):
        return row.get("id", "?"), {}
    walked = found.get("node")
    if isinstance(walked, dict):
        found = walked
    held = found.get("properties")
    if not isinstance(held, dict):
        held = found
    keyed = found.get("key") or held.get("key") or held.get("title")
    return keyed or row.get("id", "?"), held


def named(row):
    """Which page a row is, however the backend addressed it."""
    return page_in(row)[0]


def status_in(row):
    """Whether the page a row carries says it has been replaced."""
    return page_in(row)[1].get("status", "")


def summarised(text):
    """What one look-up answered, in one line."""
    rows = rows_in(text)
    if rows is None:
        return shortened(text)
    if not rows:
        return "no rows"
    listed = []
    for row in rows:
        score = row.get("score")
        mark = "!" if status_in(row) == "superseded" else ""
        listed.append(
            f"{named(row)}{mark}"
            + (f" {score:.3f}" if isinstance(score, float) else "")
        )
    counted = "1 row" if len(rows) == 1 else f"{len(rows)} rows"
    return f"{counted}: " + ", ".join(listed)


def cleared(where):
    """Removes what an earlier report wrote there, and nothing else."""
    where.mkdir(parents=True, exist_ok=True)
    for stale in [
        *where.glob("*-*-input.json"),
        *where.glob("*-*-answer.txt"),
        *where.glob("*-*-lookups.txt"),
    ]:
        stale.unlink()


def written(where, order, node, given, answer, lookups):
    """One node's input, its answer, and every look-up it made, numbered as it finished."""
    try:
        pretty = json.dumps(
            json.loads(given), indent=2, sort_keys=True, ensure_ascii=False
        )
    except json.JSONDecodeError:
        pretty = given
    (where / f"{order}-{node}-input.json").write_text(pretty + "\n", encoding="utf-8")
    (where / f"{order}-{node}-answer.txt").write_text(answer + "\n", encoding="utf-8")
    if lookups:
        said = "\n\n".join(
            f"== {call['name']} {json.dumps(call['arguments'], ensure_ascii=False)}\n{call['text']}"
            for call in lookups
        )
        (where / f"{order}-{node}-lookups.txt").write_text(
            said + "\n", encoding="utf-8"
        )


def main():
    argv = sys.argv[1:]
    where = None
    if "--answers" in argv:
        at = argv.index("--answers")
        where = pathlib.Path(argv[at + 1])
        argv = argv[:at] + argv[at + 2 :]
        cleared(where)
    path = argv[0] if argv else "out/stream.sse"

    order, given, lookups, calls = [], {}, {}, {}
    asked, superseded, followed, unread = 0, [], 0, []
    done = None
    for name, body in events(path):
        node = body.get("node", "?")
        if name == "node":
            given[node] = body["input"]
            print(f"  {node:8} begins")
        elif name == "tool_call":
            asked += 1
            calls[body["id"]] = {
                "name": body["name"],
                "arguments": body["arguments"],
                "node": node,
            }
            print(
                f"    ? {body['name']:20} {shortened(json.dumps(body['arguments'], ensure_ascii=False), 72)}"
            )
        elif name == "tool_result":
            call = calls.get(body["id"], {"name": "?", "arguments": {}, "node": node})
            call["text"] = body["text"]
            lookups.setdefault(call["node"], []).append(call)
            rows = rows_in(body["text"]) or []
            superseded.extend(
                named(row) for row in rows if status_in(row) == "superseded"
            )
            if call["name"].endswith(f"{WIRE}neighbors"):
                followed += 1
            broke = " [the tool called it a failure]" if body["failed"] else ""
            print(f"    = {summarised(body['text'])}{broke}")
        elif name == "node_done":
            spent = body["tokens"]
            order.append(node)
            if not lookups.get(node):
                unread.append(node)
            if where is not None:
                written(
                    where,
                    len(order),
                    node,
                    given.get(node, ""),
                    body["text"],
                    lookups.get(node, []),
                )
            print(
                f"  {node:8} {body['stop']:9} in {spent['input']:5}/{spent['output']:5} tokens, "
                f"{len(body['text']):5} characters out"
            )
        elif name == "node_skipped":
            print(f"  . {node} does not run: nothing reached it")
        elif name == "done":
            done = body

    if done is None:
        print("  the stream carries no `done`: the run did not finish")
        return 1
    spent = done["tokens"]
    print(f"\n  look-ups asked for:   {asked}")
    print(f"  joins followed:       {followed}")
    print(f"  superseded pages hit: {', '.join(superseded) or 'none'}")
    # Where the failure this example exists to find would appear: a node holding a look-up tool
    # and answering from its own knowledge. The stream does not say which tools a node held, so
    # every node that looked nothing up is listed and the reader knows which of them should have.
    print(f"  looked nothing up:    {', '.join(unread) or 'none'}")
    print(f"  answered with:        {', '.join(sorted(done['answer']))}")
    print(
        f"  stopped at:           {done['stopped_at'] or 'nothing — every node that ran answered'}"
    )
    print(
        f"  spent:                {spent['input']}/{spent['output']} tokens, "
        f"{done['cost_micros']} micros"
    )
    print(f"  nodes, in the order they finished: {' -> '.join(order)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
