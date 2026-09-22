# What an owner hands out inside its tenant, and what it may not hand out at all.

POST {{host}}/_ip/login
{"user": "alice", "passphrase": "{{passphrase}}"}
HTTP 204

PUT {{host}}/_ip/tenants/acme/members/bob/grants/observer
x-ip-csrf: 1
HTTP 204

# An api capability waits for access to that api.
PUT {{host}}/_ip/tenants/acme/members/bob/grants/api_adv_mgr/chat
x-ip-csrf: 1
HTTP 409
[Asserts]
body == "grant api_access first"

PUT {{host}}/_ip/tenants/acme/members/bob/grants/api_access/chat
x-ip-csrf: 1
HTTP 204

PUT {{host}}/_ip/tenants/acme/members/bob/grants/api_adv_mgr/chat
x-ip-csrf: 1
HTTP 204

# And goes only after what depends on it.
DELETE {{host}}/_ip/tenants/acme/members/bob/grants/api_access/chat
x-ip-csrf: 1
HTTP 409
[Asserts]
body == "revoke api_adv_mgr first"

GET {{host}}/_ip/tenants/acme/members/bob/grants
HTTP 200
[Asserts]
jsonpath "$" count == 3
jsonpath "$[0].capability" == "api_adv_mgr"
jsonpath "$[0].api" == "chat"
jsonpath "$[0].created_at" isInteger

GET {{host}}/_ip/tenants/acme/members/bob/grants?limit=1&offset=2
HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].capability" == "observer"

# A capability held against an account is the system administrator's to hand out.
PUT {{host}}/_ip/users/bob/grants/tenant_mgr
x-ip-csrf: 1
HTTP 403

# A capability at the wrong scope is refused whoever asks.
PUT {{host}}/_ip/tenants/acme/members/bob/grants/tenant_mgr
x-ip-csrf: 1
HTTP 422

DELETE {{host}}/_ip/tenants/acme/members/bob/grants/api_adv_mgr/chat
x-ip-csrf: 1
HTTP 204

DELETE {{host}}/_ip/tenants/acme/members/bob/grants/api_access/chat
x-ip-csrf: 1
HTTP 204
