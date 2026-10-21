# Setting what a tenant and its accounts may spend, and reading it back.

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

# A limit over a whole tenant is the administrator's to set.
PUT {{host}}/_ip/limits
x-ip-csrf: 1
{"tenant": "acme", "counted": "tokens", "period": "month", "allowance": 1000000}
HTTP 201
[Asserts]
jsonpath "$.tenant" == "acme"
jsonpath "$.counted" == "tokens"
jsonpath "$.period" == "month"
jsonpath "$.allowance" == 1000000
jsonpath "$.created_at" isInteger

PUT {{host}}/_ip/limits
x-ip-csrf: 1
{"tenant": "acme", "counted": "requests", "period": "minute", "allowance": 60}
HTTP 201

GET {{host}}/_ip/limits?tenant=acme
HTTP 200
[Asserts]
jsonpath "$" count == 2
# Newest first: the request rate was set last.
jsonpath "$[0].counted" == "requests"

# What it allows must be a limit, and the stretch must be one this server counts over.
PUT {{host}}/_ip/limits
x-ip-csrf: 1
{"tenant": "acme", "counted": "tokens", "period": "month", "allowance": 0}
HTTP 422

PUT {{host}}/_ip/limits
x-ip-csrf: 1
{"tenant": "acme", "counted": "tokens", "period": "fortnight", "allowance": 10}
HTTP 422

PUT {{host}}/_ip/limits
x-ip-csrf: 1
{"tenant": "nowhere", "counted": "tokens", "period": "month", "allowance": 10}
HTTP 404

DELETE {{host}}/_ip/limits
x-ip-csrf: 1
{"tenant": "acme", "counted": "requests", "period": "minute"}
HTTP 204

DELETE {{host}}/_ip/limits
x-ip-csrf: 1
{"tenant": "acme", "counted": "requests", "period": "minute"}
HTTP 404

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

# A tenant owner sets what one of its own accounts may spend, and nothing wider.
POST {{host}}/_ip/login
{"user": "alice", "passphrase": "{{passphrase}}"}
HTTP 204

PUT {{host}}/_ip/limits
x-ip-csrf: 1
{"tenant": "acme", "user": "bob", "counted": "tokens", "period": "day", "allowance": 5000}
HTTP 201
[Asserts]
jsonpath "$.user" == "bob"

PUT {{host}}/_ip/limits
x-ip-csrf: 1
{"tenant": "acme", "counted": "tokens", "period": "day", "allowance": 5000}
HTTP 403

PUT {{host}}/_ip/limits
x-ip-csrf: 1
{"tenant": "globex", "user": "dave", "counted": "tokens", "period": "day", "allowance": 5000}
HTTP 403

GET {{host}}/_ip/limits?tenant=globex
HTTP 403

GET {{host}}/_ip/limits
HTTP 200
[Asserts]
jsonpath "$" count == 2

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

GET {{host}}/_ip/limits
HTTP 401

# A money cap goes through the same route with no shape of its own. The allowance is micros
# of the deployment's currency, the unit a price and a usage row already hold, so five
# currency units a month is 5000000.

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

PUT {{host}}/_ip/limits
x-ip-csrf: 1
{"tenant": "acme", "counted": "money", "period": "month", "allowance": 5000000}
HTTP 201
[Asserts]
jsonpath "$.counted" == "money"
jsonpath "$.allowance" == 5000000

GET {{host}}/_ip/limits?tenant=acme
HTTP 200
[Asserts]
jsonpath "$[0].counted" == "money"

# A name no kind carries is refused rather than taken for one of them.
PUT {{host}}/_ip/limits
x-ip-csrf: 1
{"tenant": "acme", "counted": "dollars", "period": "month", "allowance": 1}
HTTP 422

DELETE {{host}}/_ip/limits
x-ip-csrf: 1
{"tenant": "acme", "counted": "money", "period": "month"}
HTTP 204

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204
