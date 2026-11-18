#!/usr/bin/env python3
"""Reads a graph run's event stream and says which branch it took, in the order it happened.

    report.py <stream.sse> [--answers <directory>]

With `--answers`, each node's input and answer is written out beside the report, numbered in
the order the nodes finished. That is how `sample-run/` is made, so it can be made again.
"""

import json
import pathlib
import sys


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
    """One line of it, short enough to read in a report."""
    text = " ".join(str(text).split())
    return text if len(text) <= most else text[: most - 3] + "..."


def notes_in(answer):
    """What a node that chose a branch said to the steps it chose.

    Read off the choosing node's own answer rather than off a `branch_taken` event: the event
    says which way the work went, while what the node *said* is its answer, which the steps it
    named are given and the stream has already carried.
    """
    try:
        said = json.loads(answer)
    except json.JSONDecodeError:
        return "— (not json: nothing was taken)"
    if not isinstance(said, dict):
        return "— (not an object: nothing was taken)"
    return shortened(
        said.get("notes", "— (no notes: the steps got the choice and nothing else)")
    )


def written(where, order, node, given, answer):
    """One node's input and answer, numbered as it finished."""
    where.mkdir(parents=True, exist_ok=True)
    try:
        pretty = json.dumps(
            json.loads(given), indent=2, sort_keys=True, ensure_ascii=False
        )
    except json.JSONDecodeError:
        pretty = given
    (where / f"{order}-{node}-input.json").write_text(pretty + "\n", encoding="utf-8")
    (where / f"{order}-{node}-answer.txt").write_text(answer + "\n", encoding="utf-8")


def main():
    argv = sys.argv[1:]
    where = None
    if "--answers" in argv:
        at = argv.index("--answers")
        where = pathlib.Path(argv[at + 1])
        argv = argv[:at] + argv[at + 2 :]
    path = argv[0] if argv else "out/stream.sse"

    runs, order, answers, given = set(), [], {}, {}
    taken, passed, skipped = [], [], []
    done = None
    for name, body in events(path):
        runs.add(body.get("run"))
        if name == "node":
            given[body["node"]] = body["input"]
            print(f"  {body['node']:9} begins on {len(body['input']):6} characters")
        elif name == "node_done":
            spent = body["tokens"]
            answers[body["node"]] = body["text"]
            order.append(body["node"])
            if where is not None:
                written(
                    where,
                    len(order),
                    body["node"],
                    given.get(body["node"], ""),
                    body["text"],
                )
            print(
                f"  {body['node']:9} {body['stop']:9} "
                f"in {spent['input']:5}/{spent['output']:5} tokens, "
                f"{len(body['text']):6} characters out"
            )
        elif name == "branch_taken":
            taken.append(body["next"])
            print(f"  > {body['node']} sends the work on to {body['next']}")
            print(f"    notes: {notes_in(answers.get(body['node'], ''))}")
        elif name == "branch_not_taken":
            passed.append(body["next"])
            print(f"  - {body['node']} passes over {body['next']}")
        elif name == "branch_refused":
            print(
                f"  x {body['node']} named {body['next']} and was refused "
                f"({body['refusal']}: {body['why']})"
            )
        elif name == "node_skipped":
            skipped.append(body["node"])
            print(f"  . {body['node']} does not run: nothing reached it")
        elif name == "done":
            done = body

    if done is None:
        print("  the stream carries no `done`: the run did not finish")
        return 1
    spent = done["tokens"]
    print(
        f"\n  branches taken:   {', '.join(taken) or 'none — the choosing node is the answer'}"
    )
    print(f"  branches passed:  {', '.join(passed) or 'none'}")
    print(f"  nodes skipped:    {', '.join(skipped) or 'none'}")
    print(f"  answered with:    {', '.join(sorted(done['answer']))}")
    print(
        f"  stopped at:       {done['stopped_at'] or 'nothing — every node that ran answered'}"
    )
    print(
        f"  spent:            {spent['input']}/{spent['output']} tokens, "
        f"{done['cost_micros']} micros"
    )
    print(f"  run id:           {', '.join(sorted(str(run) for run in runs))}")
    print(f"  nodes, in the order they finished: {' -> '.join(order)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
