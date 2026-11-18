#!/usr/bin/env python3
"""Reads a graph run's event stream and says what the loops and the branches did, in order.

    report.py <stream.sse> [--answers <directory>]

With `--answers`, each node's input and answer is written out, numbered in the order the nodes
finished — so a node that ran twice appears twice. That is how `sample-run/` is made.

The one thing to watch for in the output is a node that answers and is skipped afterwards: a
loop back puts the node that chose it back, and what it chooses on its second pass may no
longer include the step that sent the work there.
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


def shortened(text, most=88):
    """One line of it, short enough to read in a report."""
    text = " ".join(str(text).split())
    return text if len(text) <= most else text[: most - 3] + "..."


def notes_in(answer):
    """What a node that chose a branch, or sent the work back, said about it."""
    try:
        said = json.loads(answer)
    except json.JSONDecodeError:
        return "— (not json)"
    if not isinstance(said, dict):
        return "— (not an object)"
    return shortened(said.get("notes", "— (no notes)"))


def attempt_in(given):
    """Which attempt a node is on, read off the input it was given."""
    try:
        said = json.loads(given)
    except json.JSONDecodeError:
        return ""
    if not isinstance(said, dict) or "$again" not in said:
        return ""
    for who, what in said["$again"].items():
        if isinstance(what, dict) and "attempt" in what:
            return f"attempt {what['attempt']}, on what {who} said"
    return "again"


def arrived_in(given):
    """Which parents actually delivered to a node, which is what it ran on."""
    try:
        said = json.loads(given)
    except json.JSONDecodeError:
        return ""
    if not isinstance(said, dict):
        return ""
    from_ = sorted(key for key in said if not key.startswith("$"))
    return f"on {', '.join(from_)}" if from_ else "on the run's own input"


def cleared(where):
    """Removes what an earlier report wrote there, and nothing else.

    A longer run leaves higher numbers behind, and a shorter run afterwards does not overwrite
    them — so without this the directory reads as a run that never happened.
    """
    where.mkdir(parents=True, exist_ok=True)
    for stale in [*where.glob("*-*-input.json"), *where.glob("*-*-answer.txt")]:
        stale.unlink()


def written(where, order, node, given, answer):
    """One node's input and answer, numbered as it finished."""
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
        cleared(where)
    path = argv[0] if argv else "out/stream.sse"

    runs, order, answers, given = set(), [], {}, {}
    taken, passed, skipped, loops = [], [], [], 0
    done = None
    for name, body in events(path):
        runs.add(body.get("run"))
        if name == "node":
            given[body["node"]] = body["input"]
            print(
                f"  {body['node']:9} begins {arrived_in(body['input']):26}"
                f"  {attempt_in(body['input'])}"
            )
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
        elif name == "loop_back":
            loops += 1
            print(
                f"  ^ {body['node']} sends the work back to {body['back_to']}, "
                f"taken {body['taken']} of {body['cap']}"
            )
        elif name == "loop_refused":
            print(
                f"  x {body['node']} asked for {body['back_to']} and was refused "
                f"({body['refusal']}: {body['why']})"
            )
        elif name == "node_skipped":
            skipped.append(body["node"])
            ran = " — having already answered once" if body["node"] in answers else ""
            print(f"  . {body['node']} does not run: nothing reached it{ran}")
        elif name == "done":
            done = body

    if done is None:
        print("  the stream carries no `done`: the run did not finish")
        return 1
    spent = done["tokens"]
    print(f"\n  loops taken:      {loops}")
    print(f"  branches taken:   {', '.join(taken) or 'none'}")
    print(f"  branches passed:  {', '.join(passed) or 'none'}")
    print(f"  nodes skipped:    {', '.join(skipped) or 'none'}")
    print(f"  answered with:    {', '.join(sorted(done['answer']))}")
    # The whole point of the example: a node that answered and was then passed over.
    dropped = sorted(set(answers) & set(skipped))
    print(f"  ran but dropped:  {', '.join(dropped) or 'none'}")
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
