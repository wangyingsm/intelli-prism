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
# The listing carries the deployment's configured upstreams alongside what is stored, so the
# stored rule is picked out by its source rather than by where it happens to sit.
jsonpath "$[?(@.source == 'stored')]" count == 1
jsonpath "$[?(@.source == 'stored')].shadowed" nth 0 == false
jsonpath "$[?(@.source == 'stored')].key.path" nth 0 == "/v1"
jsonpath "$[?(@.source == 'stored')].target[0].host" nth 0 == "api.example.com"
jsonpath "$[?(@.source == 'stored')].created_at" nth 0 isInteger

DELETE {{host}}/_ip/routes
x-ip-csrf: 1
{"protocol": "https", "host": "gateway.local", "port": 443, "path": "/v1"}
HTTP 204

DELETE {{host}}/_ip/routes
x-ip-csrf: 1
{"protocol": "https", "host": "gateway.local", "port": 443, "path": "/v1"}
HTTP 404

# A rule standing behind several endpoints says how its requests are spread, what share
# each takes, and the hardest class each can serve. A share of nothing drains that endpoint.
PUT {{host}}/_ip/routes
x-ip-csrf: 1
{
  "api": "chat",
  "key": {"protocol": "https", "host": "gateway.local", "port": 443, "path": "/replicated"},
  "strategy": "ratio",
  "hedge": 2,
  "hedge_delay": 250,
  "target": [
    {"protocol": "https", "host": "one.example.com", "port": 443, "path": "/v1", "weight": 3, "serves": "hard"},
    {"protocol": "https", "host": "two.example.com", "port": 443, "path": "/v1"},
    {"protocol": "https", "host": "drained.example.com", "port": 443, "path": "/v1", "weight": 0}
  ]
}
HTTP 204

GET {{host}}/_ip/routes
HTTP 200
[Asserts]
jsonpath "$[?(@.key.path == '/replicated')].strategy" nth 0 == "ratio"
jsonpath "$[?(@.key.path == '/replicated')].hedge" nth 0 == 2
jsonpath "$[?(@.key.path == '/replicated')].hedge_delay" nth 0 == 250
jsonpath "$[?(@.key.path == '/replicated')].target[0].weight" nth 0 == 3
jsonpath "$[?(@.key.path == '/replicated')].target[0].serves" nth 0 == "hard"
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
jsonpath "$[?(@.key.path == '/plain')].hedge" nth 0 == 1
jsonpath "$[?(@.key.path == '/plain')].hedge_delay" count == 0
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

# A hedge past what this gateway races is refused rather than stored and ignored.
PUT {{host}}/_ip/routes
x-ip-csrf: 1
{
  "api": "chat",
  "key": {"protocol": "https", "host": "gateway.local", "port": 443, "path": "/wide"},
  "hedge": 9,
  "target": [{"protocol": "https", "host": "api.example.com", "port": 443, "path": "/v1"}]
}
HTTP 422

