# Reading the trail of who changed what. Everything the files before this one did is already
# in it — the tenants, the accounts, the grants, the routes, the plugins, the limits and the
# prices — so what is checked here is who may ask, what they must name, and the shape of a
# row. Counts that would move whenever an earlier file changes are deliberately not asserted.

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

# The system administrator reads every tenant without naming one.
GET {{host}}/_ip/audit
HTTP 200
[Asserts]
jsonpath "$" count > 0
jsonpath "$[0].actor" exists
jsonpath "$[0].through" exists
jsonpath "$[0].entity" exists
jsonpath "$[0].entity_id" exists
jsonpath "$[0].created_at" isInteger

# Narrowed to one kind of thing, and to one account. The tenants were made by the file that
# makes tenants, and root made them, so both narrowings have something to find.
GET {{host}}/_ip/audit?entity=tenant
HTTP 200
[Asserts]
jsonpath "$" count > 0
jsonpath "$[0].entity" == "tenant"

GET {{host}}/_ip/audit?actor=root
HTTP 200
[Asserts]
jsonpath "$" count > 0
jsonpath "$[0].actor" == "root"

# An account is recorded against no tenant, so naming one cannot reach it. This is the
# isolation the endpoint rests on, asked of a live server rather than of a unit test.
GET {{host}}/_ip/audit?tenant=acme&entity=user
HTTP 200
[Asserts]
jsonpath "$" count == 0

# A page of the size the caller asked for.
GET {{host}}/_ip/audit?limit=2
HTTP 200
[Asserts]
jsonpath "$" count == 2

# A kind of thing this build does not carry, and names that are not names.
GET {{host}}/_ip/audit?entity=unicorn
HTTP 422

GET {{host}}/_ip/audit?tenant=not%20a%20tenant
HTTP 422

GET {{host}}/_ip/audit?actor=not%20an%20account
HTTP 422

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

# A tenant owner names the tenant it is asking about, and reaches no other.
POST {{host}}/_ip/login
{"user": "alice", "passphrase": "{{passphrase}}"}
HTTP 204

GET {{host}}/_ip/audit
HTTP 422

GET {{host}}/_ip/audit?tenant=acme
HTTP 200
[Asserts]
jsonpath "$" count > 0
jsonpath "$[0].tenant" == "acme"

GET {{host}}/_ip/audit?tenant=globex
HTTP 403

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

# A caller that proved nothing reads nothing.
GET {{host}}/_ip/audit
HTTP 401
