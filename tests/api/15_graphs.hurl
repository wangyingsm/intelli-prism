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

# A loop back, written and read back out of the plane with the cap it was given.
#
# This is the half no stub can prove, and the reason it is here: the cap is a property of the
# *edge*, and a read plan asking a real plane for one answers null, which a scripted stub happily
# answers 2 to. What proves the cap crossed the wire is a real plane handing it back.
PUT {{host}}/_ip/graphs/looping?tenant=graphers
x-ip-csrf: 1
{
  "name": "Loops back",
  "max_nodes": 8,
  "nodes": [
    {"name": "intake", "agent": "intake"},
    {"name": "reply", "agent": "reply"}
  ],
  "edges": [{"from": "intake", "to": "reply"}],
  "loops": [{"from": "reply", "to": "intake", "cap": 2}]
}
HTTP 201
[Asserts]
jsonpath "$.loops" count == 1
jsonpath "$.loops[0].cap" == 2

GET {{host}}/_ip/graphs/looping?tenant=graphers
HTTP 200
[Asserts]
jsonpath "$.edges" count == 1
jsonpath "$.edges[0].from" == "intake"
jsonpath "$.loops" count == 1
jsonpath "$.loops[0].from" == "reply"
jsonpath "$.loops[0].to" == "intake"
jsonpath "$.loops[0].cap" == 2

DELETE {{host}}/_ip/graphs/looping?tenant=graphers
x-ip-csrf: 1
HTTP 204

# A branch, written and read back out of the plane with the condition it was given.
#
# Here for the cap's reason, one milestone on: the condition is a property of the *edge*, so what
# proves it crossed the wire is a real plane handing it back. A conditional edge is written twice —
# `FEEDS` for the ordering, `BRANCH_FEEDS` for the condition — and reading the graph joins the two
# again by the pair of names, which no stub can prove either.
PUT {{host}}/_ip/graphs/branching?tenant=graphers
x-ip-csrf: 1
{
  "name": "Branches",
  "max_nodes": 8,
  "nodes": [
    {"name": "intake", "agent": "intake"},
    {"name": "reply", "agent": "reply"}
  ],
  "edges": [{"from": "intake", "to": "reply", "condition": "the complaint needs an answer"}]
}
HTTP 201
[Asserts]
jsonpath "$.edges" count == 1
jsonpath "$.edges[0].condition" == "the complaint needs an answer"

GET {{host}}/_ip/graphs/branching?tenant=graphers
HTTP 200
[Asserts]
jsonpath "$.edges" count == 1
jsonpath "$.edges[0].from" == "intake"
jsonpath "$.edges[0].to" == "reply"
jsonpath "$.edges[0].condition" == "the complaint needs an answer"

DELETE {{host}}/_ip/graphs/branching?tenant=graphers
x-ip-csrf: 1
HTTP 204

# An edge that carries no condition carries none back: a graph written before branches existed, and
# the one case a read could have grown a `null` where there was nothing.
PUT {{host}}/_ip/graphs/unconditional?tenant=graphers
x-ip-csrf: 1
{
  "name": "Always delivers",
  "max_nodes": 4,
  "nodes": [
    {"name": "intake", "agent": "intake"},
    {"name": "reply", "agent": "reply"}
  ],
  "edges": [{"from": "intake", "to": "reply"}]
}
HTTP 201

GET {{host}}/_ip/graphs/unconditional?tenant=graphers
HTTP 200
[Asserts]
jsonpath "$.edges[0].condition" not exists

DELETE {{host}}/_ip/graphs/unconditional?tenant=graphers
x-ip-csrf: 1
HTTP 204

# A condition of nothing, which would say a branch is worth taking under no circumstances at all.
PUT {{host}}/_ip/graphs/blank-condition?tenant=graphers
x-ip-csrf: 1
{
  "name": "A branch with nothing on it",
  "max_nodes": 4,
  "nodes": [
    {"name": "intake", "agent": "intake"},
    {"name": "reply", "agent": "reply"}
  ],
  "edges": [{"from": "intake", "to": "reply", "condition": "   "}]
}
HTTP 422

# A branch naming a node the graph has not got, refused by the check every edge goes through.
PUT {{host}}/_ip/graphs/branch-to-nowhere?tenant=graphers
x-ip-csrf: 1
{
  "name": "A branch to nowhere",
  "max_nodes": 4,
  "nodes": [{"name": "intake", "agent": "intake"}],
  "edges": [{"from": "intake", "to": "nowhere", "condition": "it is worth doing"}]
}
HTTP 422

# A loop back a node declares on itself is taken: a node may always ask for its own work again,
# and a self loop closes no cycle in the feeding edges, which is what the plane is asked about.
PUT {{host}}/_ip/graphs/self-loop?tenant=graphers
x-ip-csrf: 1
{
  "name": "Asks itself again",
  "max_nodes": 4,
  "nodes": [{"name": "only", "agent": "intake"}],
  "loops": [{"from": "only", "to": "only", "cap": 1}]
}
HTTP 201

GET {{host}}/_ip/graphs/self-loop?tenant=graphers
HTTP 200
[Asserts]
jsonpath "$.loops[0].from" == "only"
jsonpath "$.loops[0].to" == "only"

DELETE {{host}}/_ip/graphs/self-loop?tenant=graphers
x-ip-csrf: 1
HTTP 204

# A loop back pointing the way the work already goes is refused, and the graph is taken back out.
#
# Whether one node's answer reaches another is reachability, which the types cannot see: `intake`
# feeds `reply`, so sending work back from `intake` to `reply` sends it forwards. The plane is
# asked for a feeding path from the target to the asker, and there is none.
PUT {{host}}/_ip/graphs/forward-loop?tenant=graphers
x-ip-csrf: 1
{
  "name": "Loops the wrong way",
  "max_nodes": 4,
  "nodes": [
    {"name": "intake", "agent": "intake"},
    {"name": "reply", "agent": "reply"}
  ],
  "edges": [{"from": "intake", "to": "reply"}],
  "loops": [{"from": "intake", "to": "reply", "cap": 1}]
}
HTTP 422

GET {{host}}/_ip/graphs/forward-loop?tenant=graphers
HTTP 404

# A cap of nothing, which would declare a loop back that may never be taken.
PUT {{host}}/_ip/graphs/never-loops?tenant=graphers
x-ip-csrf: 1
{
  "name": "A loop that may never run",
  "max_nodes": 4,
  "nodes": [{"name": "only", "agent": "intake"}],
  "loops": [{"from": "only", "to": "only", "cap": 0}]
}
HTTP 422

# A loop back naming a node the graph has not got.
PUT {{host}}/_ip/graphs/loop-to-nowhere?tenant=graphers
x-ip-csrf: 1
{
  "name": "A loop back to nowhere",
  "max_nodes": 4,
  "nodes": [{"name": "only", "agent": "intake"}],
  "loops": [{"from": "only", "to": "nowhere", "cap": 1}]
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
