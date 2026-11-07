# Running an agent template, over a live server and a real socket. What is checked here is
# the door — who may start a run and what is refused before any stream opens — and then one
# run carried to its end as an event stream, against the stub upstream `run.sh` starts.
#
# The order of the cases is the order the handler settles things in, which is why the ones
# that need a standing inside the tenant come after the login that has one: the administrator
# writes every template here but can start no run, so it never reaches the later refusals.
#
# A run's tools are not exercised here: that needs an MCP server to answer as well, and the
# resolving, the calling and the failing are covered in `crates/ip-run`'s own tests against
# a stub. What this file adds is the part no in-process test can: the events really crossing
# a socket, framed as server-sent events.

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

# An owner, because a run goes out as its caller and only a standing inside the tenant
# carries the right to reach an api.
POST {{host}}/_ip/tenants
x-ip-csrf: 1
{"tenant": "runners", "owner": "runner", "passphrase": "{{passphrase}}"}
HTTP 201

PUT {{host}}/_ip/agents/stocktake?tenant=runners
x-ip-csrf: 1
{
  "name": "Stocktake",
  "config": {
    "role": "system",
    "prompt": "Answer from what you are told.",
    "max_turns": 4,
    "time_limit": 60,
    "req_timeout": 30
  }
}
HTTP 201

PUT {{host}}/_ip/agents/needs-tools?tenant=runners
x-ip-csrf: 1
{
  "name": "Needs tools",
  "config": {
    "role": "system",
    "prompt": "Look it up.",
    "tool_set": ["nowhere.sql"],
    "max_turns": 4,
    "time_limit": 60,
    "req_timeout": 30
  }
}
HTTP 201

# A name that is not one, refused before the tenant is even read.
POST {{host}}/_ip/agents/NOT%20AN%20ID/run?tenant=runners
x-ip-csrf: 1
{"input": "how many are left?"}
HTTP 422

# A run spends money, so it is charged to a tenant even for the account that may manage the
# deployment's own templates without naming one.
POST {{host}}/_ip/agents/stocktake/run
x-ip-csrf: 1
{"input": "how many are left?"}
HTTP 422
[Asserts]
body contains "charged"

# The administrator manages every tenant's templates but is a member of none, so it holds no
# standing inside one to run as, and a run it started would have nobody to charge.
POST {{host}}/_ip/agents/stocktake/run?tenant=runners
x-ip-csrf: 1
{"input": "how many are left?"}
HTTP 403

# From here as the owner, who does hold a standing inside the tenant.
POST {{host}}/_ip/login
{"user": "runner", "passphrase": "{{passphrase}}"}
HTTP 204

# A template naming a tool no registered server offers is refused before the stream opens,
# so a caller reads a status code rather than one event inside a 200 saying it went wrong.
POST {{host}}/_ip/agents/needs-tools/run?tenant=runners
x-ip-csrf: 1
{"input": "how many are left?"}
HTTP 422
[Asserts]
body contains "nowhere"

POST {{host}}/_ip/agents/nothing-here/run?tenant=runners
x-ip-csrf: 1
{"input": "how many are left?"}
HTTP 404

# A body without the one field a run cannot be started without.
POST {{host}}/_ip/agents/stocktake/run?tenant=runners
x-ip-csrf: 1
{}
HTTP 422

# A browser that sends its cookie without the header a page on another site cannot forge.
POST {{host}}/_ip/agents/stocktake/run?tenant=runners
{"input": "how many are left?"}
HTTP 403

POST {{host}}/_ip/agents/stocktake/run?tenant=runners
x-ip-csrf: 1
{"input": "how many are left?"}
HTTP 200
[Asserts]
header "content-type" startsWith "text/event-stream"
body contains "event: turn"
body contains "event: text"
body contains "event: done"
body contains "four crates are left"
body contains "\"stop\":\"answered\""

# The run reached the upstream through the gateway like any other request, so it was metered
# and recorded the way a proxied call is.
GET {{host}}/_ip/usage?tenant=runners
HTTP 200
[Asserts]
jsonpath "$" count > 0
jsonpath "$[0].api" == "anthropic"

# A template another owner holds is not found rather than refused, so a caller is never told
# the difference between a thing that is not theirs and a thing that is not there.
POST {{host}}/_ip/agents/triage/run?tenant=runners
x-ip-csrf: 1
{"input": "how many are left?"}
HTTP 404
