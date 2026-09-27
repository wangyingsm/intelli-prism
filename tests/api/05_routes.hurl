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

# A rule standing behind several endpoints says how its requests are spread, and what share
# each takes. A share of nothing drains that endpoint.
PUT {{host}}/_ip/routes
x-ip-csrf: 1
{
  "api": "chat",
  "key": {"protocol": "https", "host": "gateway.local", "port": 443, "path": "/replicated"},
  "strategy": "ratio",
  "target": [
    {"protocol": "https", "host": "one.example.com", "port": 443, "path": "/v1", "weight": 3},
    {"protocol": "https", "host": "two.example.com", "port": 443, "path": "/v1"},
    {"protocol": "https", "host": "drained.example.com", "port": 443, "path": "/v1", "weight": 0}
  ]
}
HTTP 204

GET {{host}}/_ip/routes
HTTP 200
[Asserts]
jsonpath "$[?(@.key.path == '/replicated')].strategy" nth 0 == "ratio"
jsonpath "$[?(@.key.path == '/replicated')].target[0].weight" nth 0 == 3
jsonpath "$[?(@.key.path == '/replicated')].target[1].weight" nth 0 == 1
jsonpath "$[?(@.key.path == '/replicated')].target[2].weight" nth 0 == 0

# A rule that says neither dispatches in turn, with an equal share each.
PUT {{host}}/_ip/routes
x-ip-csrf: 1
{
  "api": "chat",
  "key": {"protocol": "https", "host": "gateway.local", "port": 443, "path": "/plain"},
  "target": [{"protocol": "https", "host": "api.example.com", "port": 443, "path": "/v1"}]
}
HTTP 204

GET {{host}}/_ip/routes
HTTP 200
[Asserts]
jsonpath "$[?(@.key.path == '/plain')].strategy" nth 0 == "round_robin"
jsonpath "$[?(@.key.path == '/plain')].target[0].weight" nth 0 == 1

# A strategy this gateway does not carry is refused rather than stored and ignored.
PUT {{host}}/_ip/routes
x-ip-csrf: 1
{
  "api": "chat",
  "key": {"protocol": "https", "host": "gateway.local", "port": 443, "path": "/coin"},
  "strategy": "coin_toss",
  "target": [{"protocol": "https", "host": "api.example.com", "port": 443, "path": "/v1"}]
}
HTTP 422

