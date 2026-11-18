# Retrievers, over a live server. What is checked here is who may manage whose, what the types
# refuse at the door, and that a key goes in and never comes back out — not counts, which would
# move whenever an earlier file changes. Nothing looks anything up yet, so there is nothing else
# to see.
#
# **Every retriever written here offers the tools that walk edges and no `search`**, because this
# suite's server is built without the `semantic` feature and so embeds nothing: a `search` is the
# one tool whose width a registration compares against the embedder's, and one offered to a
# deployment that has none is refused — which this file checks for its own sake, further down.

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

# The deployment's own, written by the only account that may.
PUT {{host}}/_ip/retrievers/handbook
x-ip-csrf: 1
{
  "name": "The handbook",
  "endpoint": "https://drsg.acme.test:8443/rpc",
  "auth": {"kind": "key", "key": "qd-live-9f2c4a7b1e"},
  "source": {"kind": "drsg", "plane": "handbook", "property": "embedding"},
  "tools": ["neighbors", "rank"],
  "meta": {
    "team": "Which team the page belongs to",
    "quarter": "Which quarter the figure is for, as 2026Q1"
  },
  "max_hits": 8
}
HTTP 201
[Asserts]
jsonpath "$.id" == "handbook"
jsonpath "$.name" == "The handbook"
jsonpath "$.tenant" not exists
jsonpath "$.endpoint" == "https://drsg.acme.test:8443/rpc"
jsonpath "$.source.kind" == "drsg"
jsonpath "$.source.plane" == "handbook"
jsonpath "$.tools" contains "rank"
# Nothing was asked of the backend, so nothing is reported as measured.
jsonpath "$.measured" not exists
jsonpath "$.meta.team" == "Which team the page belongs to"
jsonpath "$.max_hits" == 8
jsonpath "$.created_at" isInteger
# The credential: its kind is reported, it is not, and there is no field it could be in.
jsonpath "$.auth" == "key"
jsonpath "$.key" not exists
jsonpath "$.auth.key" not exists
body not contains "qd-live-9f2c4a7b1e"

GET {{host}}/_ip/retrievers/handbook
HTTP 200
[Asserts]
jsonpath "$.meta.quarter" == "Which quarter the figure is for, as 2026Q1"
jsonpath "$.auth" == "key"
body not contains "qd-live-9f2c4a7b1e"

GET {{host}}/_ip/retrievers
HTTP 200
[Asserts]
jsonpath "$[?(@.id == 'handbook')]" count == 1
body not contains "qd-live-9f2c4a7b1e"

# Nor does the trail, which is the one other place a credential could settle.
GET {{host}}/_ip/audit?entity=retriever
HTTP 200
[Asserts]
jsonpath "$[0].entity" == "retriever"
jsonpath "$[0].entity_id" == "handbook"
body not contains "qd-live-9f2c4a7b1e"

# A plane is the only kind that answers the four tools that walk edges, and a label narrows what
# it reads.
PUT {{host}}/_ip/retrievers/graph
x-ip-csrf: 1
{
  "name": "The code graph",
  "endpoint": "http://127.0.0.1:44453",
  "source": {"kind": "drsg", "plane": "handbook", "label": "Document", "property": "embedding"},
  "tools": ["neighbors", "rank", "relate", "cluster"]
}
HTTP 201
[Asserts]
jsonpath "$.source.plane" == "handbook"
jsonpath "$.source.label" == "Document"
jsonpath "$.tools" count == 4
jsonpath "$.auth" == "none"
jsonpath "$.max_hits" == 5

# Written again under the same name, replacing what was there rather than sitting beside it.
PUT {{host}}/_ip/retrievers/graph
x-ip-csrf: 1
{
  "name": "The code graph, related only",
  "endpoint": "http://127.0.0.1:44453",
  "source": {"kind": "drsg", "plane": "handbook", "property": "embedding"},
  "tools": ["relate"]
}
HTTP 201
[Asserts]
jsonpath "$.name" == "The code graph, related only"
jsonpath "$.tools" count == 1
jsonpath "$.source.label" not exists

GET {{host}}/_ip/retrievers
HTTP 200
[Asserts]
jsonpath "$[?(@.id == 'graph')]" count == 1

# What the types refuse, each for its own reason: a collection asked for a tool that needs edges
# a collection has not got, a backend this deployment does not speak, a retriever offering
# nothing, a ceiling past what one look-up may answer with, an endpoint this gateway cannot
# call, a payload field whose name is not a key, and a field this build does not carry.
PUT {{host}}/_ip/retrievers/bad
x-ip-csrf: 1
{
  "name": "Bad",
  "endpoint": "https://qdrant.acme.test:6333",
  "source": {"kind": "qdrant", "collection": "handbook"},
  "tools": ["search", "cluster"]
}
HTTP 422
[Asserts]
body contains "cluster"

PUT {{host}}/_ip/retrievers/bad
x-ip-csrf: 1
{
  "name": "Bad",
  "endpoint": "https://qdrant.acme.test:6333",
  "source": {"kind": "milvus", "collection": "handbook"},
  "tools": ["search"]
}
HTTP 422

PUT {{host}}/_ip/retrievers/bad
x-ip-csrf: 1
{
  "name": "Bad",
  "endpoint": "https://qdrant.acme.test:6333",
  "source": {"kind": "qdrant", "collection": "handbook"},
  "tools": []
}
HTTP 422

PUT {{host}}/_ip/retrievers/bad
x-ip-csrf: 1
{
  "name": "Bad",
  "endpoint": "https://qdrant.acme.test:6333",
  "source": {"kind": "qdrant", "collection": "handbook"},
  "tools": ["search"],
  "max_hits": 900
}
HTTP 422

PUT {{host}}/_ip/retrievers/bad
x-ip-csrf: 1
{
  "name": "Bad",
  "endpoint": "ftp://qdrant.acme.test",
  "source": {"kind": "qdrant", "collection": "handbook"},
  "tools": ["search"]
}
HTTP 422

PUT {{host}}/_ip/retrievers/bad
x-ip-csrf: 1
{
  "name": "Bad",
  "endpoint": "https://qdrant.acme.test:6333",
  "source": {"kind": "qdrant", "collection": "handbook"},
  "tools": ["search"],
  "meta": {"not a key": "what it holds"}
}
HTTP 422

PUT {{host}}/_ip/retrievers/bad
x-ip-csrf: 1
{
  "name": "Bad",
  "endpoint": "https://qdrant.acme.test:6333",
  "source": {"kind": "qdrant", "collection": "handbook"},
  "tools": ["search"],
  "nearness": 0.9
}
HTTP 422

# A `search` is the one tool whose width is compared against the embedder's, and this deployment
# has none: so it is refused here, while an operator is reading the answer, rather than at search
# time inside a run they are paying for. Both kinds, because neither can be asked.
PUT {{host}}/_ip/retrievers/searching
x-ip-csrf: 1
{
  "name": "Searching",
  "endpoint": "https://qdrant.acme.test:6333",
  "source": {"kind": "qdrant", "collection": "handbook"},
  "tools": ["search"]
}
HTTP 422
[Asserts]
body contains "embeds nothing"
body contains "semantic"

PUT {{host}}/_ip/retrievers/searching
x-ip-csrf: 1
{
  "name": "Searching",
  "endpoint": "http://127.0.0.1:44453",
  "source": {"kind": "drsg", "plane": "handbook", "property": "embedding"},
  "tools": ["search", "rank"]
}
HTTP 422
[Asserts]
body contains "embeds nothing"

# And it was refused rather than half-written.
GET {{host}}/_ip/retrievers/searching
HTTP 404

# An id that is not one, which the path carries and the type refuses.
PUT {{host}}/_ip/retrievers/not%20an%20id
x-ip-csrf: 1
{
  "name": "Bad",
  "endpoint": "https://qdrant.acme.test:6333",
  "source": {"kind": "qdrant", "collection": "handbook"},
  "tools": ["search"]
}
HTTP 422

GET {{host}}/_ip/retrievers/nobody-wrote-this
HTTP 404

DELETE {{host}}/_ip/retrievers/graph
x-ip-csrf: 1
HTTP 204

DELETE {{host}}/_ip/retrievers/graph
x-ip-csrf: 1
HTTP 404

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

# A tenant owner manages its own tenant's by standing, and one id names a different corpus in
# each tenant: this is the whole of ruling 4.
POST {{host}}/_ip/login
{"user": "alice", "passphrase": "{{passphrase}}"}
HTTP 204

PUT {{host}}/_ip/retrievers/handbook?tenant=acme
x-ip-csrf: 1
{
  "name": "Acme's handbook",
  "endpoint": "https://drsg.acme.test:8443/rpc",
  "source": {"kind": "drsg", "plane": "acme-handbook", "property": "embedding"},
  "tools": ["neighbors"]
}
HTTP 201
[Asserts]
jsonpath "$.tenant" == "acme"
jsonpath "$.source.plane" == "acme-handbook"

GET {{host}}/_ip/retrievers?tenant=acme
HTTP 200
[Asserts]
jsonpath "$[?(@.id == 'handbook')]" count == 1
jsonpath "$[?(@.source.plane == 'handbook')]" count == 0

# The deployment's own is not a tenant's, and a tenant owner does not manage it.
GET {{host}}/_ip/retrievers
HTTP 422

# And reaches no other tenant, by either half of the api.
GET {{host}}/_ip/retrievers?tenant=globex
HTTP 403

GET {{host}}/_ip/retrievers/handbook?tenant=globex
HTTP 403

PUT {{host}}/_ip/retrievers/theirs?tenant=globex
x-ip-csrf: 1
{
  "name": "Theirs",
  "endpoint": "https://qdrant.acme.test:6333",
  "source": {"kind": "qdrant", "collection": "theirs"},
  "tools": ["search"]
}
HTTP 403

DELETE {{host}}/_ip/retrievers/theirs?tenant=globex
x-ip-csrf: 1
HTTP 403

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

# A member of the tenant holding `observer` but not `sys_agent` manages none of it: an endpoint
# and a payload schema say where a tenant keeps its documents.
#
# The file that writes the templates granted bob `sys_agent` and left it held, and the suite is
# one server read in file order — so the capability goes before the refusal is asked for, or
# what this checks is an earlier file's last line.
POST {{host}}/_ip/login
{"user": "alice", "passphrase": "{{passphrase}}"}
HTTP 204

DELETE {{host}}/_ip/tenants/acme/members/bob/grants/sys_agent
x-ip-csrf: 1
HTTP 204

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

POST {{host}}/_ip/login
{"user": "bob", "passphrase": "{{passphrase}}"}
HTTP 204

GET {{host}}/_ip/retrievers?tenant=acme
HTTP 403

PUT {{host}}/_ip/retrievers/bobs?tenant=acme
x-ip-csrf: 1
{
  "name": "Bob's",
  "endpoint": "https://qdrant.acme.test:6333",
  "source": {"kind": "qdrant", "collection": "bobs"},
  "tools": ["search"]
}
HTTP 403

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

# Granted `sys_agent`, the same member manages them: whoever may write the templates may
# describe what they read.
POST {{host}}/_ip/login
{"user": "alice", "passphrase": "{{passphrase}}"}
HTTP 204

PUT {{host}}/_ip/tenants/acme/members/bob/grants/sys_agent
x-ip-csrf: 1
HTTP 204

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

POST {{host}}/_ip/login
{"user": "bob", "passphrase": "{{passphrase}}"}
HTTP 204

GET {{host}}/_ip/retrievers?tenant=acme
HTTP 200
[Asserts]
jsonpath "$[?(@.id == 'handbook')]" count == 1

PUT {{host}}/_ip/retrievers/bobs?tenant=acme
x-ip-csrf: 1
{
  "name": "Bob's",
  "endpoint": "https://drsg.acme.test:8443/rpc",
  "source": {"kind": "drsg", "plane": "bobs", "property": "embedding"},
  "tools": ["cluster"]
}
HTTP 201

DELETE {{host}}/_ip/retrievers/bobs?tenant=acme
x-ip-csrf: 1
HTTP 204

# A granted capability does not reach the deployment's own.
GET {{host}}/_ip/retrievers
HTTP 422

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

# A caller that proved nothing manages nothing.
GET {{host}}/_ip/retrievers
HTTP 401

PUT {{host}}/_ip/retrievers/anon
x-ip-csrf: 1
{
  "name": "Anon",
  "endpoint": "https://qdrant.acme.test:6333",
  "source": {"kind": "qdrant", "collection": "anon"},
  "tools": ["search"]
}
HTTP 401
