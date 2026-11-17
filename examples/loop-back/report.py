#!/usr/bin/env python3
"""Reads a graph run's event stream and says what the loop back did, in the order it happened."""

import json
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
    """What a node that sent the work back wrote about it.

    Read off the asking node's own answer rather than the `loop_back` event: the event says
    which way the work went and how much of the cap is left, while what the asker *said* is
    its answer, which the stream has already carried.
    """
    try:
        asked = json.loads(answer)
    except json.JSONDecodeError:
        return "—"
    if not isinstance(asked, dict):
        return "—"
    return shortened(asked.get("notes", "—"))


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


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "out/stream.sse"
    runs, order, answers = set(), [], {}
    loops = 0
    done = None
    for name, body in events(path):
        runs.add(body.get("run"))
        if name == "node":
            print(
                f"  {body['node']:7} begins on {len(body['input']):6} characters"
                f"  {attempt_in(body['input'])}"
            )
        elif name == "node_done":
            spent = body["tokens"]
            answers[body["node"]] = body["text"]
            print(
                f"  {body['node']:7} {body['stop']:9} "
                f"in {spent['input']:5}/{spent['output']:5} tokens, "
                f"{len(body['text']):6} characters out"
            )
            order.append(body["node"])
        elif name == "loop_back":
            loops += 1
            print(
                f"  * {body['node']} sends the work back to {body['back_to']}, "
                f"taken {body['taken']} of {body['cap']}"
            )
            print(f"    notes: {notes_in(answers.get(body['node'], ''))}")
        elif name == "loop_refused":
            print(
                f"  x {body['node']} asked for {body['back_to']} and was refused "
                f"({body['refusal']}: {body['why']})"
            )
        elif name == "done":
            done = body

    if done is None:
        print("  the stream carries no `done`: the run did not finish")
        return 1
    spent = done["tokens"]
    print(f"\n  loops taken:   {loops}")
    print(f"  answered with: {', '.join(sorted(done['answer']))}")
    print(f"  stopped at:    {done['stopped_at'] or 'nothing — every node answered'}")
    print(
        f"  spent:         {spent['input']}/{spent['output']} tokens, {done['cost_micros']} micros"
    )
    print(f"  run id:        {', '.join(sorted(str(run) for run in runs))}")
    print(f"  nodes, in the order they finished: {' -> '.join(order)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
