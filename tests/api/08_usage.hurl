# Reading what was spent. Nothing has been proxied in this run, so the lists are empty; what
# is checked here is who may ask and what they must name.

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

# The system administrator sees every tenant without naming one.
GET {{host}}/_ip/usage
HTTP 200
[Asserts]
jsonpath "$" count == 0

GET {{host}}/_ip/usage?tenant=acme&limit=5
HTTP 200
[Asserts]
jsonpath "$" count == 0

GET {{host}}/_ip/usage?tenant=not%20a%20tenant
HTTP 422

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

# A tenant owner names the tenant it is asking about, and reaches no other.
POST {{host}}/_ip/login
{"user": "alice", "passphrase": "{{passphrase}}"}
HTTP 204

GET {{host}}/_ip/usage
HTTP 422

GET {{host}}/_ip/usage?tenant=acme
HTTP 200

GET {{host}}/_ip/usage?tenant=globex
HTTP 403

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

# A caller that proved nothing reads nothing.
GET {{host}}/_ip/usage
HTTP 401

# The summary of what was spent answers behind the same door as the list it summarizes:
# the same names, the same owners, and — nothing having been proxied — zero of both
# numbers.

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

GET {{host}}/_ip/usage/summary
HTTP 200
[Asserts]
jsonpath "$.answered" == 0
jsonpath "$.sent" == 0
jsonpath "$.since" == null
# The shape of the per-level counts, which is what a client would break on; this
# ledger is empty, so what they add up to is the gateway suite's subject.
jsonpath "$.decided.request" == 0
jsonpath "$.decided.turn" == 0
jsonpath "$.decided.classifier" == 0
jsonpath "$.decided.arbiter" == 0
jsonpath "$.decided.undecided" == 0
# An empty ledger cost nothing and left nothing unpriced; the two keys travel together,
# so a client reading the bill knows how much of it is missing.
jsonpath "$.cost_micros" == 0
jsonpath "$.unpriced" == 0

GET {{host}}/_ip/usage/summary?tenant=acme
HTTP 200

GET {{host}}/_ip/usage/summary?tenant=not%20a%20tenant
HTTP 422

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

POST {{host}}/_ip/login
{"user": "alice", "passphrase": "{{passphrase}}"}
HTTP 204

GET {{host}}/_ip/usage/summary
HTTP 422

GET {{host}}/_ip/usage/summary?tenant=globex
HTTP 403

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

GET {{host}}/_ip/usage/summary
HTTP 401
