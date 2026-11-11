#!/usr/bin/env python3
"""Reads a graph run's event stream and says what the graph did, in the order it happened."""

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


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "out/stream.sse"
    runs, nodes, done = set(), [], None
    for name, body in events(path):
        runs.add(body.get("run"))
        if name == "node":
            print(
                f"  {body['node']:11} begins on {len(body['input']):6} characters of input"
            )
        elif name == "node_done":
            spent = body["tokens"]
            print(
                f"  {body['node']:11} {body['stop']:9} "
                f"in {spent['input']:5}/{spent['output']:5} tokens, "
                f"{len(body['text']):6} characters out"
            )
            nodes.append(body["node"])
        elif name == "done":
            done = body

    if done is None:
        print("  the stream carries no `done`: the run did not finish")
        return 1
    spent = done["tokens"]
    print(f"\n  answered with: {', '.join(sorted(done['answer']))}")
    print(f"  stopped at:    {done['stopped_at'] or 'nothing — every node answered'}")
    print(
        f"  spent:         {spent['input']}/{spent['output']} tokens, {done['cost_micros']} micros"
    )
    print(f"  run id:        {', '.join(sorted(str(run) for run in runs))}")
    print(f"  nodes, in the order they finished: {' → '.join(nodes)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
