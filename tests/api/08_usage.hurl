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
