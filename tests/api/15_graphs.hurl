# Run graphs, over a live server and a real dr-strange plane. What is checked here is what no
# in-process test can: the graph really crossing the wire into a store outside this process, and
# coming back out of it the same shape. Who may manage whose, and what the types refuse at the
# door, are covered by `routes/graph.rs`'s own tests against a stub plane.
#
# `run.sh` starts the store, on a database of its own that goes when the run does, and leaves
# this file out entirely when `drsg` is not on the path.

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

# A tenant to own them, and a member who may write them.
POST {{host}}/_ip/tenants
x-ip-csrf: 1
{"tenant": "graphers", "owner": "grapher", "passphrase": "{{passphrase}}"}
HTTP 201

# The templates the nodes name. A graph may only name templates its own owner holds.
PUT {{host}}/_ip/agents/intake?tenant=graphers
x-ip-csrf: 1
{"name": "Intake", "config": {"role": "system", "prompt": "Sort it.", "max_turns": 2, "time_limit": 60, "req_timeout": 30}}
HTTP 201

PUT {{host}}/_ip/agents/reply?tenant=graphers
x-ip-csrf: 1
{"name": "Reply", "config": {"role": "system", "prompt": "Answer it.", "max_turns": 2, "time_limit": 60, "req_timeout": 30}}
HTTP 201

POST {{host}}/_ip/login
{"user": "grapher", "passphrase": "{{passphrase}}"}
HTTP 204

# Written, and answered with what was stored rather than with what was sent.
PUT {{host}}/_ip/graphs/triage?tenant=graphers
x-ip-csrf: 1
{
  "name": "Triage",
  "max_nodes": 8,
  "nodes": [
    {"name": "intake", "agent": "intake"},
    {"name": "reply", "agent": "reply"}
  ],
  "edges": [{"from": "intake", "to": "reply"}]
}
HTTP 201
[Asserts]
jsonpath "$.id" == "triage"
jsonpath "$.name" == "Triage"
jsonpath "$.tenant" == "graphers"
jsonpath "$.nodes" count == 2
jsonpath "$.edges" count == 1
jsonpath "$.max_nodes" == 8

# Read back out of the plane, which is the half a stub cannot prove.
GET {{host}}/_ip/graphs/triage?tenant=graphers
HTTP 200
[Asserts]
jsonpath "$.nodes[0].name" == "intake"
jsonpath "$.nodes[1].agent" == "reply"
jsonpath "$.edges[0].from" == "intake"
jsonpath "$.edges[0].to" == "reply"

GET {{host}}/_ip/graphs?tenant=graphers
HTTP 200
[Asserts]
jsonpath "$[?(@.id == 'triage')]" count == 1

# Written again under the same name, replacing what was there rather than sitting beside it.
PUT {{host}}/_ip/graphs/triage?tenant=graphers
x-ip-csrf: 1
{
  "name": "Triage, second pass",
  "max_nodes": 4,
  "nodes": [{"name": "intake", "agent": "intake"}]
}
HTTP 201
[Asserts]
jsonpath "$.name" == "Triage, second pass"
jsonpath "$.nodes" count == 1
jsonpath "$.edges" count == 0

GET {{host}}/_ip/graphs?tenant=graphers
HTTP 200
[Asserts]
jsonpath "$[?(@.id == 'triage')]" count == 1

# A graph that is not there is not found, and says nothing about whose it would have been.
GET {{host}}/_ip/graphs/absent?tenant=graphers
HTTP 404

DELETE {{host}}/_ip/graphs/absent?tenant=graphers
x-ip-csrf: 1
HTTP 404

# A node naming a template nobody stored is **taken**, and refused when the graph is run.
#
# That is deliberate rather than a gap, and `16_graph_runs.hurl` shows why: a template can be
# deleted after a graph names it, so "every node names a template that exists" is not an
# invariant this store can hold. Checking it at write time would buy an early error for a typo
# and charge an ordering constraint for it, while still letting the reference rot afterwards.
# The run is the one place the check can be made to mean something, and that is where it is.
PUT {{host}}/_ip/graphs/unknown-agent?tenant=graphers
x-ip-csrf: 1
{
  "name": "Names a template nobody stored",
  "max_nodes": 4,
  "nodes": [{"name": "only", "agent": "not-a-template"}]
}
HTTP 201

GET {{host}}/_ip/graphs/unknown-agent?tenant=graphers
HTTP 200
[Asserts]
jsonpath "$.nodes[0].agent" == "not-a-template"

DELETE {{host}}/_ip/graphs/unknown-agent?tenant=graphers
x-ip-csrf: 1
HTTP 204

# A graph that feeds itself is refused, because an order cannot be taken of it.
PUT {{host}}/_ip/graphs/cyclic?tenant=graphers
x-ip-csrf: 1
{
  "name": "Feeds itself",
  "max_nodes": 4,
  "nodes": [
    {"name": "intake", "agent": "intake"},
    {"name": "reply", "agent": "reply"}
  ],
  "edges": [
    {"from": "intake", "to": "reply"},
    {"from": "reply", "to": "intake"}
  ]
}
HTTP 422

GET {{host}}/_ip/graphs/cyclic?tenant=graphers
HTTP 404

# More nodes than the graph's own ceiling allows.
PUT {{host}}/_ip/graphs/too-big?tenant=graphers
x-ip-csrf: 1
{
  "name": "Over its own ceiling",
  "max_nodes": 1,
  "nodes": [
    {"name": "intake", "agent": "intake"},
    {"name": "reply", "agent": "reply"}
  ]
}
HTTP 422

# An edge naming a node the graph has not got.
PUT {{host}}/_ip/graphs/dangling?tenant=graphers
x-ip-csrf: 1
{
  "name": "An edge to nowhere",
  "max_nodes": 4,
  "nodes": [{"name": "intake", "agent": "intake"}],
  "edges": [{"from": "intake", "to": "nowhere"}]
}
HTTP 422

# A browser that sends its cookie without the header a page on another site cannot forge.
PUT {{host}}/_ip/graphs/forged?tenant=graphers
{"name": "Forged", "max_nodes": 2, "nodes": [{"name": "only", "agent": "intake"}]}
HTTP 403

# Deleted, and then gone from the store rather than only from the listing.
DELETE {{host}}/_ip/graphs/triage?tenant=graphers
x-ip-csrf: 1
HTTP 204

GET {{host}}/_ip/graphs/triage?tenant=graphers
HTTP 404

GET {{host}}/_ip/graphs?tenant=graphers
HTTP 200
[Asserts]
jsonpath "$[?(@.id == 'triage')]" count == 0
