# Creating tenants, each with the account that owns it.

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

POST {{host}}/_ip/tenants
x-ip-csrf: 1
{"tenant": "acme", "owner": "alice", "passphrase": "{{passphrase}}"}
HTTP 201
[Asserts]
jsonpath "$.tenant" == "acme"
jsonpath "$.owner" == "alice"
jsonpath "$.created_at" isInteger
# The tenant's key is never in an answer: it is shown in the web ui alone.
body not contains "key"

POST {{host}}/_ip/tenants
x-ip-csrf: 1
{"tenant": "globex", "owner": "dave", "passphrase": "{{passphrase}}"}
HTTP 201

POST {{host}}/_ip/tenants
x-ip-csrf: 1
{"tenant": "acme", "owner": "other", "passphrase": "{{passphrase}}"}
HTTP 409

POST {{host}}/_ip/tenants
x-ip-csrf: 1
{"tenant": "not a tenant id", "owner": "someone", "passphrase": "{{passphrase}}"}
HTTP 422

GET {{host}}/_ip/tenants/acme
HTTP 200
[Asserts]
jsonpath "$.tenant" == "acme"
jsonpath "$.created_at" isInteger

GET {{host}}/_ip/tenants/nowhere
HTTP 404
