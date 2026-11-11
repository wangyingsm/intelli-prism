# Running a graph, over a live server and a real socket. What is checked here is the door —
# who may start a run and what is refused before any stream opens — and then one graph carried
# to its end as an event stream, against the stub upstream `run.sh` starts.
#
# The order of the cases is the order the handler settles things in: the administrator writes
# every graph here and can start no run, because a run is charged to a tenant and it is a member
# of none. A real model and real benchmarks are a separate thing, in `examples/graph-run`.

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
