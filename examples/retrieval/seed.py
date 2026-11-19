#!/usr/bin/env python3
"""Puts one corpus into both backends, embedded by the deployment's own artefact.

    seed.py --artifact <dir> --corpus corpus.json \
            --qdrant http://127.0.0.1:6333 --collection handbook \
            --drsg http://127.0.0.1:18139/rpc --token <token> --plane handbook

**The embeddings have to come from the artefact the gateway itself loads.** A look-up embeds the
model's query with `IP_EMBED_ARTIFACT` and sends the numbers to the backend, so a corpus embedded
by anything else is being searched in somebody else's geometry — the scores come back looking
perfectly reasonable and mean nothing. That is why this reads the same directory and repeats what
`ip_embed::runtime::Embedder::embed` does, step for step: the manifest's `query_prefix` in front of
every text, the tokenizer beside it, mean pooling over every token, then one unit long.

The prefix is applied to documents as well as to queries, which is not what e5 was trained for —
see the README. It is what the gateway does, and matching it matters more than being right.
"""

import argparse
import json
import pathlib
import urllib.error
import urllib.request

import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer

#: What a vector property must be written as for a plane to hold it as a vector.
#:
#: Written plainly it is stored as an array of numbers, which reads back as itself, is not indexed,
#: and makes every search answer nothing at all rather than fail.
VECTOR = "$vector"


class Encoder:
    """The deployment's artefact, loaded the way the gateway loads it."""

    def __init__(self, directory):
        where = pathlib.Path(directory)
        self.manifest = json.loads((where / "embed.json").read_text(encoding="utf-8"))
        self.tokenizer = Tokenizer.from_file(str(where / "tokenizer.json"))
        self.session = ort.InferenceSession(
            str(where / "model.onnx"), providers=["CPUExecutionProvider"]
        )
        self.inputs = {one.name for one in self.session.get_inputs()}

    @property
    def width(self):
        """How many numbers one vector holds, as the manifest declares it."""
        return int(self.manifest["width"])

    def embed(self, text):
        """What one text means, as a list of floats exactly one unit long."""
        prefixed = self.manifest.get("query_prefix", "") + text
        ids = np.array([self.tokenizer.encode(prefixed).ids], dtype=np.int64)
        feed = {"input_ids": ids, "attention_mask": np.ones_like(ids)}
        if "token_type_ids" in self.inputs:
            feed["token_type_ids"] = np.zeros_like(ids)
        answer = self.session.run(None, feed)[0]
        pooled = answer[0].mean(axis=0) if answer.ndim == 3 else answer[0]
        if self.manifest.get("pooling") == "first" and answer.ndim == 3:
            pooled = answer[0][0]
        unit = pooled / np.linalg.norm(pooled)
        return [float(number) for number in unit]


def asked(url, body=None, method="GET", token=None):
    """One http call, and the whole answer it made."""
    data = None if body is None else json.dumps(body).encode("utf-8")
    request = urllib.request.Request(url, data=data, method=method)
    request.add_header("content-type", "application/json")
    if token:
        request.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(request, timeout=60) as answer:
            return json.loads(answer.read() or b"null")
    except urllib.error.HTTPError as refused:
        raise SystemExit(
            f"{method} {url} answered {refused.code}: {refused.read().decode('utf-8', 'replace')}"
        ) from refused


def rpc(endpoint, token, method, params):
    """One JSON-RPC call to a plane, refusing anything it reports as an error."""
    answer = asked(
        endpoint,
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params},
        method="POST",
        token=token,
    )
    if "error" in answer:
        raise SystemExit(f"{method}: {answer['error']}")
    return answer["result"]


def into_collection(where, collection, pages, vectors, width):
    """The corpus as qdrant points, in a collection made for this width.

    Cosine, because that is what a plane assumes and cannot be told otherwise: a collection that
    scored by dot product would rank the same corpus differently from the plane beside it, and the
    example would be comparing two geometries rather than two backends.
    """
    asked(f"{where}/collections/{collection}", method="DELETE")
    asked(
        f"{where}/collections/{collection}",
        {"vectors": {"size": width, "distance": "Cosine"}},
        method="PUT",
    )
    points = [
        {
            "id": at + 1,
            "vector": vectors[page["key"]],
            "payload": {
                "key": page["key"],
                "title": page["title"],
                "text": page["text"],
                "team": page["team"],
                "status": page["status"],
            },
        }
        for at, page in enumerate(pages)
    ]
    asked(
        f"{where}/collections/{collection}/points?wait=true",
        {"points": points},
        method="PUT",
    )
    return len(points)


def into_plane(endpoint, token, plane, corpus, vectors):
    """The same corpus as plane nodes, with the joins between them.

    The joins are the half a collection has not got, and `SUPERSEDED_BY` is the one this example
    turns on: a page that has been replaced is still the nearest thing to the question, and only
    the edge leaving it says what to answer with instead.
    """
    held = {listed.get("name") for listed in rpc(endpoint, token, "plane.list", {})}
    if plane not in held:
        rpc(endpoint, token, "plane.create", {"name": plane})
    for page in corpus["pages"]:
        # A plane offers no drop and refuses a key it already holds, so a second seeding of one
        # store has to clear the page before writing it.
        rpc(endpoint, token, "node.delete", {"plane": plane, "key": page["key"]})
        rpc(
            endpoint,
            token,
            "node.create",
            {
                "plane": plane,
                "key": page["key"],
                "labels": [corpus["label"]],
                "properties": {
                    "title": page["title"],
                    "text": page["text"],
                    "team": page["team"],
                    "status": page["status"],
                    corpus["property"]: {VECTOR: vectors[page["key"]]},
                },
            },
        )
    for edge in corpus["edges"]:
        rpc(
            endpoint,
            token,
            "edge.create",
            {
                "plane": plane,
                "src": edge["from"],
                "dst": edge["to"],
                "type": edge["type"],
                "properties": {},
            },
        )
    return len(corpus["pages"]), len(corpus["edges"])


def main():
    parse = argparse.ArgumentParser(description=__doc__)
    parse.add_argument("--artifact", required=True)
    parse.add_argument("--corpus", required=True)
    parse.add_argument("--qdrant", required=True)
    parse.add_argument("--collection", required=True)
    parse.add_argument("--drsg", required=True)
    parse.add_argument("--token", required=True)
    parse.add_argument("--plane", required=True)
    said = parse.parse_args()

    corpus = json.loads(pathlib.Path(said.corpus).read_text(encoding="utf-8"))
    encoder = Encoder(said.artifact)
    vectors = {
        page["key"]: encoder.embed(f"{page['title']}. {page['text']}")
        for page in corpus["pages"]
    }
    print(f"  embedded {len(vectors)} pages, {encoder.width} numbers each")

    points = into_collection(
        said.qdrant, said.collection, corpus["pages"], vectors, encoder.width
    )
    print(f"  collection {said.collection}: {points} points")

    nodes, edges = into_plane(said.drsg, said.token, said.plane, corpus, vectors)
    print(f"  plane {said.plane}: {nodes} nodes, {edges} joins")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
