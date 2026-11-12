#!/usr/bin/env python3
"""Reads one cluster graph run and says what each node did and which member did it.

The stream alone cannot answer the second half: a dispatched node's events are
indistinguishable from a local node's, which is the point of the dispatch. Where the work
went is in the plane, so this reads both and puts them side by side.
"""

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


def rows(answer):
    """The rows of a `plane.query` answer, whichever shape this store version returned."""
    result = answer.get("result", answer)
    listed = result if isinstance(result, list) else result.get("rows", [])
    return [row.get("properties", row) for row in listed]


def placements(path):
    """Which member each node ran on, by node name.

    Built from every sample taken while the run was going, because a placement is cleared
    when its node ends: one sample at the end would show nothing running anywhere, which
    is true and useless. A node seen once is kept, so a sample that missed a short node
    does not erase it.
    """
    seen = {}
    try:
        with open(path, encoding="utf-8") as sampled:
            for line in sampled:
                line = line.strip()
                if not line:
                    continue
                try:
                    answer = json.loads(line)
                except json.JSONDecodeError:
                    continue
                for row in rows(answer):
                    if row.get("node") and row.get("address"):
                        seen.setdefault(row["node"], row["address"])
    except OSError:
        return {}
    return seen


def main():
    stream = sys.argv[1] if len(sys.argv) > 1 else "out/stream.sse"
    placed = placements(sys.argv[2] if len(sys.argv) > 2 else "out/placements.json")
    coordinator = sys.argv[3] if len(sys.argv) > 3 else ""
    kept = sys.argv[4] if len(sys.argv) > 4 else ""

    runs, order, done = set(), [], None
    for name, body in events(stream):
        runs.add(body.get("run"))
        if name == "node_done":
            node = body["node"]
            spent = body["tokens"]
            where = placed.get(node)
            if where is None:
                # Every sample of the plane missed it, which a node quicker than the
                # sampling interval can be. The stream still proves it ran.
                where, ran = "not sampled", "-"
            else:
                ran = "here" if where == coordinator else "crossed"
            print(
                f"  {node:9} {body['stop']:9} on {where:20} {ran:11} "
                f"{spent['input']:5}/{spent['output']:5} tokens"
            )
            order.append(node)
        elif name == "done":
            done = body

    if done is None:
        print("  the stream carries no `done`: the run did not finish")
        return 1

    spent = done["tokens"]
    print(f"\n  answered with: {', '.join(sorted(done['answer']))}")
    print(f"  stopped at:    {done['stopped_at'] or 'nothing — every node answered'}")
    print(
        f"  spent:         {spent['input']}/{spent['output']} tokens, "
        f"{done['cost_micros']} micros"
    )
    print(f"  run id:        {', '.join(sorted(str(run) for run in runs))}")
    print(f"  finished in:   {' → '.join(order)}")

    elsewhere = sorted({at for at in placed.values() if at != coordinator})
    print(
        f"\n  members that ran a node: {', '.join(sorted(set(placed.values()))) or 'none'}"
    )
    print(f"  the coordinator was:     {coordinator or 'not given'}")
    if elsewhere:
        print(f"  nodes crossed to:        {', '.join(elsewhere)}")
    elif placed:
        print(
            "  nothing crossed a machine boundary — every sampled node ran on the\n"
            "  coordinator, which is what one live member means"
        )
    else:
        print(
            "  no placement was sampled, so this says nothing either way about where\n"
            "  the nodes ran — each member's own log does"
        )
    if kept:
        try:
            with open(kept, encoding="utf-8") as keys:
                held = [line.strip() for line in keys if line.strip()]
        except OSError:
            held = []
        print(f"  answers kept in the cache: {len(held)}")
        for key in held:
            print(f"    {key}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
