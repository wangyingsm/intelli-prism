# Agent templates, over a live server. What is checked here is who may manage whose, what the
# types refuse at the door, and the shape of a stored template — not counts, which would move
# whenever an earlier file changes. Nothing runs a template, so there is nothing else to see.

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

# The deployment's own, written by the only account that may.
PUT {{host}}/_ip/agents/triage
x-ip-csrf: 1
{
  "name": "Triage",
  "sandbox": {
    "bin": {"python": "s3://mybucket/bin/python"},
    "notes.md": "s3://mybucket/notes.md"
  },
  "config": {
    "role": "system",
    "prompt": "Sort what comes in and answer what you can.",
    "tool_set": ["sql", "search"],
    "max_turns": 8,
    "time_limit": 300,
    "req_timeout": 60,
    "best_effort": "aggr"
  }
}
HTTP 201
[Asserts]
jsonpath "$.id" == "triage"
jsonpath "$.name" == "Triage"
jsonpath "$.tenant" not exists
jsonpath "$.sandbox.bin.python" == "s3://mybucket/bin/python"
jsonpath "$.sandbox['notes.md']" == "s3://mybucket/notes.md"
jsonpath "$.config.tool_set" contains "sql"
jsonpath "$.config.best_effort" == "aggr"
jsonpath "$.created_at" isInteger

GET {{host}}/_ip/agents/triage
HTTP 200
[Asserts]
jsonpath "$.config.max_turns" == 8

GET {{host}}/_ip/agents
HTTP 200
[Asserts]
jsonpath "$[?(@.id == 'triage')]" count == 1

# Written again under the same name, replacing what was there rather than sitting beside it.
PUT {{host}}/_ip/agents/triage
x-ip-csrf: 1
{
  "name": "Triage, second pass",
  "config": {"role": "system", "prompt": "Be brief.", "max_turns": 2, "time_limit": 60, "req_timeout": 10}
}
HTTP 201
[Asserts]
jsonpath "$.name" == "Triage, second pass"
jsonpath "$.sandbox.bin" not exists

GET {{host}}/_ip/agents
HTTP 200
[Asserts]
jsonpath "$[?(@.id == 'triage')]" count == 1

# What the types refuse, each for its own reason: a request that may outlive the run it is
# part of, a way of picking that is not one of the three, a path that climbs out of the
# sandbox, a name padded with whitespace, and a field this build does not carry.
PUT {{host}}/_ip/agents/bad
x-ip-csrf: 1
{"name": "Bad", "config": {"role": "system", "prompt": "x", "max_turns": 2, "time_limit": 10, "req_timeout": 60}}
HTTP 422

PUT {{host}}/_ip/agents/bad
x-ip-csrf: 1
{"name": "Bad", "config": {"role": "system", "prompt": "x", "max_turns": 2, "time_limit": 60, "req_timeout": 10, "best_effort": "guess"}}
HTTP 422

PUT {{host}}/_ip/agents/bad
x-ip-csrf: 1
{"name": "Bad", "sandbox": {"..": "s3://mybucket/x"}, "config": {"role": "system", "prompt": "x", "max_turns": 2, "time_limit": 60, "req_timeout": 10}}
HTTP 422

PUT {{host}}/_ip/agents/bad
x-ip-csrf: 1
{"name": " Bad", "config": {"role": "system", "prompt": "x", "max_turns": 2, "time_limit": 60, "req_timeout": 10}}
HTTP 422

PUT {{host}}/_ip/agents/bad
x-ip-csrf: 1
{"name": "Bad", "owner": "acme", "config": {"role": "system", "prompt": "x", "max_turns": 2, "time_limit": 60, "req_timeout": 10}}
HTTP 422

# A name that is not one, refused before anything is stored.
PUT {{host}}/_ip/agents/not%20an%20id
x-ip-csrf: 1
{"name": "Bad", "config": {"role": "system", "prompt": "x", "max_turns": 2, "time_limit": 60, "req_timeout": 10}}
HTTP 422

GET {{host}}/_ip/agents/bad
HTTP 404

# The administrator may name a tenant. One id names a different template under each owner.
PUT {{host}}/_ip/agents/triage?tenant=acme
x-ip-csrf: 1
{"name": "Acme's triage", "config": {"role": "user", "prompt": "Answer as the customer would want.", "max_turns": 4, "time_limit": 120, "req_timeout": 30}}
HTTP 201
[Asserts]
jsonpath "$.tenant" == "acme"
jsonpath "$.config.role" == "user"

GET {{host}}/_ip/agents/triage
HTTP 200
[Asserts]
jsonpath "$.name" == "Triage, second pass"

# A tenant nobody created holds nothing, rather than reading as the deployment itself.
GET {{host}}/_ip/agents?tenant=nobody
HTTP 404

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

# A tenant owner manages its own tenant's by standing, and must say which tenant that is.
POST {{host}}/_ip/login
{"user": "alice", "passphrase": "{{passphrase}}"}
HTTP 204

GET {{host}}/_ip/agents
HTTP 422

GET {{host}}/_ip/agents?tenant=acme
HTTP 200
[Asserts]
jsonpath "$[?(@.id == 'triage')]" count == 1

PUT {{host}}/_ip/agents/mine?tenant=acme
x-ip-csrf: 1
{"name": "Mine", "config": {"role": "system", "prompt": "x", "max_turns": 2, "time_limit": 60, "req_timeout": 10}}
HTTP 201

DELETE {{host}}/_ip/agents/mine?tenant=acme
x-ip-csrf: 1
HTTP 204

GET {{host}}/_ip/agents/mine?tenant=acme
HTTP 404

DELETE {{host}}/_ip/agents/mine?tenant=acme
x-ip-csrf: 1
HTTP 404

# And reaches no other tenant, by either half of the api.
GET {{host}}/_ip/agents?tenant=globex
HTTP 403

PUT {{host}}/_ip/agents/theirs?tenant=globex
x-ip-csrf: 1
{"name": "Theirs", "config": {"role": "system", "prompt": "x", "max_turns": 2, "time_limit": 60, "req_timeout": 10}}
HTTP 403

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

# A member of the tenant holding `observer` but not `sys_agent` manages none of it.
POST {{host}}/_ip/login
{"user": "bob", "passphrase": "{{passphrase}}"}
HTTP 204

GET {{host}}/_ip/agents?tenant=acme
HTTP 403

PUT {{host}}/_ip/agents/bobs?tenant=acme
x-ip-csrf: 1
{"name": "Bob's", "config": {"role": "system", "prompt": "x", "max_turns": 2, "time_limit": 60, "req_timeout": 10}}
HTTP 403

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

# Granted it, the same member manages them. This is `sys_agent`'s first endpoint.
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

GET {{host}}/_ip/agents?tenant=acme
HTTP 200
[Asserts]
jsonpath "$[?(@.id == 'triage')]" count == 1

PUT {{host}}/_ip/agents/bobs?tenant=acme
x-ip-csrf: 1
{"name": "Bob's", "config": {"role": "system", "prompt": "x", "max_turns": 2, "time_limit": 60, "req_timeout": 10}}
HTTP 201

DELETE {{host}}/_ip/agents/bobs?tenant=acme
x-ip-csrf: 1
HTTP 204

# A granted capability does not reach the deployment's own.
GET {{host}}/_ip/agents
HTTP 422

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

# A caller that proved nothing manages nothing.
GET {{host}}/_ip/agents
HTTP 401

PUT {{host}}/_ip/agents/anon
x-ip-csrf: 1
{"name": "Anon", "config": {"role": "system", "prompt": "x", "max_turns": 2, "time_limit": 60, "req_timeout": 10}}
HTTP 401
