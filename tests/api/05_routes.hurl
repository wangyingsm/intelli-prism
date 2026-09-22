# The routing rules the database holds, which only the system administrator manages.

POST {{host}}/_ip/login
{"user": "alice", "passphrase": "{{passphrase}}"}
HTTP 204

GET {{host}}/_ip/routes
HTTP 403

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

PUT {{host}}/_ip/routes
x-ip-csrf: 1
{
  "api": "chat",
  "key": {"protocol": "https", "host": "gateway.local", "port": 443, "path": "/v1"},
  "target": [{"protocol": "https", "host": "api.example.com", "port": 443, "path": "/v1"}]
}
HTTP 204

# The gateway keeps its own prefix: a stored rule inside it would stop the next start.
PUT {{host}}/_ip/routes
x-ip-csrf: 1
{
  "api": "chat",
  "key": {"protocol": "https", "host": "gateway.local", "port": 443, "path": "/_ip/own"},
  "target": [{"protocol": "https", "host": "api.example.com", "port": 443, "path": "/v1"}]
}
HTTP 422

GET {{host}}/_ip/routes
HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].source" == "stored"
jsonpath "$[0].shadowed" == false
jsonpath "$[0].key.path" == "/v1"
jsonpath "$[0].target[0].host" == "api.example.com"
jsonpath "$[0].created_at" isInteger

DELETE {{host}}/_ip/routes
x-ip-csrf: 1
{"protocol": "https", "host": "gateway.local", "port": 443, "path": "/v1"}
HTTP 204

DELETE {{host}}/_ip/routes
x-ip-csrf: 1
{"protocol": "https", "host": "gateway.local", "port": 443, "path": "/v1"}
HTTP 404
