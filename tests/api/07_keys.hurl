# A key is shown to a logged in session that gives its passphrase again, and to nothing else.

POST {{host}}/_ip/login
{"user": "alice", "passphrase": "{{passphrase}}"}
HTTP 204

POST {{host}}/_ip/keys/tenant
x-ip-csrf: 1
{"passphrase": "not the passphrase"}
HTTP 403

POST {{host}}/_ip/keys/tenant
x-ip-csrf: 1
{"passphrase": "{{passphrase}}"}
HTTP 200
[Asserts]
header "cache-control" == "no-store"
jsonpath "$.tenant" == "acme"
jsonpath "$.key" matches /^[0-9a-f]{32}$/

POST {{host}}/_ip/keys/user
x-ip-csrf: 1
{"tenant": "acme", "passphrase": "{{passphrase}}"}
HTTP 200
[Asserts]
jsonpath "$.user" == "alice"
jsonpath "$.key" matches /^[0-9a-f]{64}$/

# A tenant the caller is not in reads as one that is not there.
POST {{host}}/_ip/keys/user
x-ip-csrf: 1
{"tenant": "globex", "passphrase": "{{passphrase}}"}
HTTP 404

# A member sees its own key, never its tenant's.
POST {{host}}/_ip/login
{"user": "bob", "passphrase": "{{passphrase}}"}
HTTP 204

POST {{host}}/_ip/keys/tenant
x-ip-csrf: 1
{"passphrase": "{{passphrase}}"}
HTTP 403

POST {{host}}/_ip/keys/user
x-ip-csrf: 1
{"tenant": "acme", "passphrase": "{{passphrase}}"}
HTTP 200
[Asserts]
jsonpath "$.user" == "bob"
