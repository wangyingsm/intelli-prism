# Running a graph, over a live server and a real socket. What is checked here is the door —
# who may start a run and what is refused before any stream opens — and then two graphs carried
# to their end as an event stream, against the stub upstream `run.sh` starts: a diamond, where
# every node runs, and a triage, where what a node answers decides which of them do.
#
# The order of the cases is the order the handler settles things in: the administrator writes
# every graph here and can start no run, because a run is charged to a tenant and it is a member
# of none. A real model and real benchmarks are a separate thing, in `examples/graph-run` and
# `examples/branch`.

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

POST {{host}}/_ip/tenants
x-ip-csrf: 1
{"tenant": "runs", "owner": "runs-owner", "passphrase": "{{passphrase}}"}
HTTP 201

PUT {{host}}/_ip/agents/first?tenant=runs
x-ip-csrf: 1
{"name": "First", "config": {"role": "system", "prompt": "Say it.", "max_turns": 2, "time_limit": 60, "req_timeout": 30}}
HTTP 201

PUT {{host}}/_ip/agents/second?tenant=runs
x-ip-csrf: 1
{"name": "Second", "config": {"role": "system", "prompt": "Say it again.", "max_turns": 2, "time_limit": 60, "req_timeout": 30}}
HTTP 201

# A diamond: one root, two nodes that run at once, one that is given both their answers.
PUT {{host}}/_ip/graphs/diamond?tenant=runs
x-ip-csrf: 1
{
  "name": "Diamond",
  "max_nodes": 8,
  "nodes": [
    {"name": "root", "agent": "first"},
    {"name": "left", "agent": "second"},
    {"name": "right", "agent": "second"},
    {"name": "join", "agent": "first"}
  ],
  "edges": [
    {"from": "root", "to": "left"},
    {"from": "root", "to": "right"},
    {"from": "left", "to": "join"},
    {"from": "right", "to": "join"}
  ]
}
HTTP 201

# The administrator may write a graph and start no run: a run spends money, so it is charged to
# a tenant, and this account is a member of none.
POST {{host}}/_ip/graph_run/diamond?tenant=runs
x-ip-csrf: 1
{"input": "how many are left?"}
HTTP 403

# Naming no tenant at all is the request's own omission, and is refused before the store is asked.
POST {{host}}/_ip/graph_run/diamond
x-ip-csrf: 1
{"input": "how many are left?"}
HTTP 422

# From here as the owner, who does hold a standing inside the tenant.
POST {{host}}/_ip/login
{"user": "runs-owner", "passphrase": "{{passphrase}}"}
HTTP 204

# A graph that is not there, which says nothing about whose it would have been.
POST {{host}}/_ip/graph_run/absent?tenant=runs
x-ip-csrf: 1
{"input": "how many are left?"}
HTTP 404

# A body with no input to run on.
POST {{host}}/_ip/graph_run/diamond?tenant=runs
x-ip-csrf: 1
{}
HTTP 422

# A browser that sends its cookie without the header a page on another site cannot forge.
POST {{host}}/_ip/graph_run/diamond?tenant=runs
{"input": "how many are left?"}
HTTP 403

# A node naming a template that has been taken away since the graph was written is refused
# before the stream opens, so a caller reads a status code rather than a stream that stops.
DELETE {{host}}/_ip/agents/second?tenant=runs
x-ip-csrf: 1
HTTP 204

POST {{host}}/_ip/graph_run/diamond?tenant=runs
x-ip-csrf: 1
{"input": "how many are left?"}
HTTP 422
[Asserts]
body contains "left"
body contains "second"

PUT {{host}}/_ip/agents/second?tenant=runs
x-ip-csrf: 1
{"name": "Second", "config": {"role": "system", "prompt": "Say it again.", "max_turns": 2, "time_limit": 60, "req_timeout": 30}}
HTTP 201

# And then the whole graph, really crossing a socket as server-sent events. Four nodes in three
# generations: each says it has begun and then that it is over, every turn-level event names the
# node that took it, and the graph says `done` exactly once at the end.
POST {{host}}/_ip/graph_run/diamond?tenant=runs
x-ip-csrf: 1
{"input": "how many are left?"}
HTTP 200
[Asserts]
header "content-type" startsWith "text/event-stream"
body contains "event: node"
body contains "event: node_done"
body contains "event: turn"
body contains "event: done"
body contains "\"node\":\"root\""
body contains "\"node\":\"left\""
body contains "\"node\":\"right\""
body contains "\"node\":\"join\""
# The graph answers with the node nothing reads from, and with no other.
body contains "\"answer\":{\"join\""
# Nothing stopped it, and the run it belongs to is named on its events.
body contains "\"stopped_at\":null"
body contains "\"run\":"

# And a graph that branches, which is the only place in this suite where what a node answers
# decides what runs. `triage` declares two branches, is told so in a prompt the engine writes
# rather than the operator, and the stub plays a node that names `answer`: so one branch is taken,
# `escalate` is skipped without ever reaching the upstream, and the graph answers with the path it
# actually took. Whether a *model* judges a condition well is `examples/branch`'s question.
PUT {{host}}/_ip/graphs/triage?tenant=runs
x-ip-csrf: 1
{
  "name": "Triage",
  "max_nodes": 8,
  "nodes": [
    {"name": "triage", "agent": "first"},
    {"name": "answer", "agent": "second"},
    {"name": "escalate", "agent": "second"}
  ],
  "edges": [
    {"from": "triage", "to": "answer", "condition": "the complaint can be answered as it stands"},
    {"from": "triage", "to": "escalate", "condition": "the complaint names a loss"}
  ]
}
HTTP 201

POST {{host}}/_ip/graph_run/triage?tenant=runs
x-ip-csrf: 1
{"input": "how many are left?"}
HTTP 200
[Asserts]
header "content-type" startsWith "text/event-stream"
# The branch it took, the branch it passed over, and the node that therefore never ran.
body contains "event: branch_taken"
body contains "event: branch_not_taken"
body contains "event: node_skipped"
body contains "\"next\":\"answer\""
body contains "\"next\":\"escalate\""
# The node that was named took a turn; the one that was not never reached the upstream at all,
# which a turn event is what would say. This is the assertion the whole milestone is for.
body contains "\"node\":\"answer\",\"number\":1"
body not contains "\"node\":\"escalate\",\"number\""
# It ran on what arrived, keyed by the node that chose to send it.
body contains "\"node\":\"answer\",\"run\""
body contains "{\\\"triage\\\":"
# And the graph answered with the realized run's leaf, alone.
body contains "\"answer\":{\"answer\":\"four crates are left\"}"
body contains "\"stopped_at\":null"
