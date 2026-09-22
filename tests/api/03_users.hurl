# An owner filling its tenant, and taking an account out of it again.

POST {{host}}/_ip/login
{"user": "alice", "passphrase": "{{passphrase}}"}
HTTP 204

POST {{host}}/_ip/users
x-ip-csrf: 1
{"user": "bob", "tenant": "acme", "passphrase": "{{passphrase}}"}
HTTP 201
[Asserts]
jsonpath "$.user" == "bob"
jsonpath "$.kind" == "regular"
jsonpath "$.tenants[0].tenant" == "acme"
jsonpath "$.tenants[0].standing" == "member"

POST {{host}}/_ip/users
x-ip-csrf: 1
{"user": "carol", "tenant": "acme", "passphrase": "{{passphrase}}"}
HTTP 201

# An owner reaches no tenant but its own.
POST {{host}}/_ip/users
x-ip-csrf: 1
{"user": "erin", "tenant": "globex", "passphrase": "{{passphrase}}"}
HTTP 403

GET {{host}}/_ip/users/bob
HTTP 200
[Asserts]
jsonpath "$.tenants" count == 1

# Newest first, a page at a time.
GET {{host}}/_ip/tenants/acme/members?limit=2
HTTP 200
[Asserts]
jsonpath "$" count == 2
jsonpath "$[0].user" == "carol"
jsonpath "$[1].user" == "bob"
jsonpath "$[0].created_at" isInteger

GET {{host}}/_ip/tenants/acme/members?limit=2&offset=2
HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].user" == "alice"
jsonpath "$[0].standing" == "owner"

# Taking an account out says how much it held inside.
DELETE {{host}}/_ip/tenants/acme/members/carol
x-ip-csrf: 1
HTTP 200
[Asserts]
jsonpath "$.user" == "carol"
jsonpath "$.revoked" == 0

PUT {{host}}/_ip/tenants/acme/members/carol
x-ip-csrf: 1
HTTP 201

PUT {{host}}/_ip/tenants/acme/members/carol
x-ip-csrf: 1
HTTP 204

# An owner leaves only with its tenant.
DELETE {{host}}/_ip/tenants/acme/members/alice
x-ip-csrf: 1
HTTP 409
